//! The node's resource sampler: what it measures of its own machine, and the
//! cadence that keeps the store's sample current.
//!
//! # Why a cadence of its own, beside housekeeping
//!
//! It rides the same period for the same reason housekeeping's clock is the
//! awareness constant: the readings' freshness bound is derived from the
//! awareness interval, and a sampler running slower than the bound it feeds
//! would report `stale` of itself forever. It publishes into the store's
//! readings registry, and the greeting answers peers from there; the registry
//! and its reasons for existing live in `tessari-storage`'s own module.
//!
//! # Why a seam
//!
//! `Probe` is the one platform-boundary in this feature: production builds it
//! on `sysinfo`, tests build it on fakes that answer nothing, or everything,
//! or one figure — the honesty cases (a platform that cannot measure reports
//! the figure absent, never a zero) are only provable against a machine that
//! can be told not to know.

use tessari_storage::Readings;

/// What takes one sample. One method, because a probe that can be asked for
/// anything more is a probe a caller can shape the answer with.
pub(crate) trait Probe: Send + 'static {
    /// Measure now. Any figure the platform cannot give is absent.
    fn measure(&mut self) -> Readings;
}

/// The production probe: `sysinfo` across the platforms the engine ships on.
///
/// The disk figures name the volume the store lives on — *where a full store
/// would hurt*, not the root of everything — matched by longest mount point
/// prefix at measure time, because mounts move and a cached answer would be a
/// reading about a yesterday's arrangement. The network figures are summed
/// across every interface, because a node's load is the sum of what its
/// surfaces carry. A platform that will not answer — `System`, `Disks` or
/// `Networks` failing to build — takes its figures, not the probe: the
/// fields are absent, and the next measure tries again.
pub(crate) struct Machine {
    system: Option<sysinfo::System>,
    disks: Option<sysinfo::Disks>,
    networks: Option<sysinfo::Networks>,
    volume: Option<std::path::PathBuf>,
    /// When this probe last refreshed the CPU usage, against which the next
    /// figure is a real delta. `sysinfo` declines to update usage inside its
    /// own minimum interval and leaves the initialisation zero in place, and
    /// reporting that zero would be the invented number the sampler refuses.
    cpu_refreshed_at: Option<std::time::Instant>,
    /// When this probe last refreshed the network counters. The counters are
    /// exact deltas and carry no minimum window of their own, so the only
    /// rule is that one measure must have passed before a rate means
    /// anything — the first window after a process starts is absent, the
    /// same statement `cpu_busy` makes.
    networks_refreshed_at: Option<std::time::Instant>,
}

impl Machine {
    /// A probe measuring `volume`'s disk, and the machine's CPU and memory.
    /// `volume` is `None` for a store that lives in memory: there is no
    /// volume, and the disk figures are absent rather than about the wrong
    /// disk.
    pub(crate) fn new(volume: Option<std::path::PathBuf>) -> Self {
        let system = sysinfo::System::new().ok().map(|mut system| {
            // Populates the CPU list and dates the baseline the first real
            // delta stands on. Without this call the list stays empty and
            // every reading is the zero nothing was measured against.
            system.refresh_cpu_usage();
            system
        });
        // Mount points are absolute, so a relative volume would prefix-match
        // nothing and report absence about a disk that is there. A path that
        // cannot be resolved takes its figures instead of guessing.
        let volume = volume.and_then(|path| path.canonicalize().map(Some).unwrap_or(None));
        let networks = sysinfo::Networks::new().ok().map(|mut networks| {
            // The counter snapshot the first measured rate stands on.
            networks.refresh(false);
            networks
        });
        Self {
            system,
            disks: sysinfo::Disks::new().ok(),
            networks,
            volume,
            cpu_refreshed_at: None,
            networks_refreshed_at: None,
        }
    }

    /// The CPU busy figure, or `None` when no real delta stands behind one:
    /// either this probe has not refreshed before, or the last refresh is
    /// still inside `sysinfo`'s minimum update interval.
    fn cpu_busy(&mut self) -> Option<u64> {
        let system = self.system.as_mut()?;
        system.refresh_cpu_usage();
        let figure = match self.cpu_refreshed_at {
            Some(at) if at.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL => {
                // Basis points of one percent. The value is clamped to the
                // percentage range and rounded before the conversion, so the
                // cast is total on what reaches it; a non-finite value cannot
                // be stated honestly and takes the figure.
                let points = f64::from(system.global_cpu_usage()).clamp(0.0, 100.0) * 100.0;
                let rounded = points.round();
                if rounded.is_finite() {
                    // bgv-allow(numeric-as-cast, cast_possible_truncation, cast_sign_loss):
                    // stable std has no float-to-integer TryFrom; the value is clamped
                    // to 0..=10_000, rounded and checked finite above, so the cast is
                    // total on what reaches it.
                    #[allow(
                        clippy::as_conversions,
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss
                    )]
                    let basis_points = rounded as u64;
                    Some(basis_points)
                } else {
                    None
                }
            }
            _ => None,
        };
        self.cpu_refreshed_at = Some(std::time::Instant::now());
        figure
    }

    /// Memory total and used, both from the same refresh.
    fn memory(&mut self) -> (Option<u64>, Option<u64>) {
        match self.system.as_mut() {
            Some(system) => {
                system.refresh_memory();
                (Some(system.total_memory()), Some(system.used_memory()))
            }
            None => (None, None),
        }
    }

    /// The store volume's total and free bytes, matched by longest mount
    /// prefix at read time.
    fn disk(&mut self) -> (Option<u64>, Option<u64>) {
        let (Some(disks), Some(root)) = (self.disks.as_mut(), self.volume.as_deref()) else {
            return (None, None);
        };
        // `true` drops mounts that vanished since the last pass, so a stale
        // volume cannot keep answering after it was unmounted.
        disks.refresh(true);
        disks
            .iter()
            .filter(|disk| root.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
            .map_or((None, None), |disk| {
                (Some(disk.total_space()), Some(disk.available_space()))
            })
    }

    /// Network throughput in and out, in bytes per second over the window
    /// since this probe's last refresh, summed across every interface. The
    /// first window after a process starts is absent — a rate needs a window,
    /// and the window at start has no before.
    fn network(&mut self) -> (Option<u64>, Option<u64>) {
        let Some(networks) = self.networks.as_mut() else {
            return (None, None);
        };
        networks.refresh(false);
        let (received, transmitted) =
            networks
                .values()
                .fold((0_u64, 0_u64), |(rx, tx), interface| {
                    (
                        rx.saturating_add(interface.received()),
                        tx.saturating_add(interface.transmitted()),
                    )
                });
        let figure = self
            .networks_refreshed_at
            .map(|_| (Some(received), Some(transmitted)));
        self.networks_refreshed_at = Some(std::time::Instant::now());
        figure.unwrap_or((None, None))
    }
}

impl Probe for Machine {
    fn measure(&mut self) -> Readings {
        let cpu_busy = self.cpu_busy();
        let (memory_total, memory_used) = self.memory();
        let (disk_total, disk_free) = self.disk();
        let (network_received, network_transmitted) = self.network();
        Readings {
            cpu_cores: std::thread::available_parallelism()
                .ok()
                .and_then(|cores| u64::try_from(cores.get()).ok()),
            cpu_busy,
            memory_total,
            memory_used,
            disk_total,
            disk_free,
            network_received,
            network_transmitted,
        }
    }
}

/// Keep the store's own sample current, one measurement per pass, until the
/// node stops. A measurement that fails takes the figure, not the cadence:
/// the next pass measures again, and `INFO FOR NODE` renders what is there —
/// including, honestly, nothing.
pub(crate) async fn sample(
    store: tessari_storage::Store,
    mut probe: impl Probe,
    stop: tokio_util::sync::CancellationToken,
) {
    tessari_wire::every(
        "sampling",
        std::time::Duration::from_secs(tessari_constants::AWARENESS_SECONDS),
        &stop,
        async move |_| {
            store.readings_sampled(probe.measure());
            Ok(())
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A probe that serves one fixed sample forever — the shape a platform
    /// that knows everything takes.
    struct Fixed(Readings);

    impl Probe for Fixed {
        fn measure(&mut self) -> Readings {
            self.0
        }
    }

    #[test]
    fn the_machine_probe_measures_what_this_machine_holds() {
        // A smoke test against the real machine, asserting only what is true
        // on any machine this suite runs on: it has at least one core, some
        // memory, and the volume this workspace lives on has space on it.
        let mut probe = Machine::new(Some(std::path::PathBuf::from(".")));
        let taken = probe.measure();
        assert!(
            taken.cpu_cores.is_some_and(|cores| cores > 0),
            "a machine presents at least one core"
        );
        assert!(
            taken.memory_total.is_some_and(|memory| memory > 0),
            "a machine holds some memory"
        );
        assert_eq!(
            taken.cpu_busy, None,
            "the very first measure has no delta yet and reports none"
        );
        let taken = probe.measure();
        assert!(
            taken.cpu_busy.is_none_or(|busy| busy <= 10_000),
            "a figure this soon is either absent — the minimum update interval \
             has not passed — or a real delta in basis points"
        );
        assert!(
            taken.disk_total.is_some_and(|disk| disk > 0),
            "the workspace's volume is measurable"
        );
    }

    #[test]
    fn an_in_memory_store_has_no_volume_and_reports_no_disk() {
        let mut probe = Machine::new(None);
        let taken = probe.measure();
        assert_eq!(taken.disk_total, None, "no volume is no disk figure");
        assert_eq!(taken.disk_free, None);
    }

    #[test]
    fn the_cadence_publishes_what_the_probe_measures() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        let _guard = runtime.enter();
        let store = runtime
            .block_on(tessaridb::Db::in_memory())
            .expect("an in-memory store")
            .store()
            .clone();
        let stop = tokio_util::sync::CancellationToken::new();
        let sampling = tokio::spawn(sample(
            store.clone(),
            Fixed(Readings {
                cpu_cores: Some(2),
                cpu_busy: None,
                memory_total: Some(1_000),
                memory_used: None,
                disk_total: None,
                disk_free: None,
                network_received: Some(64_000),
                network_transmitted: None,
            }),
            stop.clone(),
        ));
        // The cadence's first pass runs at once; give the runtime a beat to
        // run it, then stop the cadence and join it.
        runtime.block_on(async {
            tokio::task::yield_now().await;
            stop.cancel();
            let _ = sampling.await;
        });
        let (sample, age) = store.readings().expect("the cadence published a sample");
        assert_eq!(sample.cpu_cores, Some(2));
        assert_eq!(sample.memory_total, Some(1_000));
        assert!(age < std::time::Duration::from_secs(60));
    }
}
