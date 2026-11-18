//! The topology a cluster declares its realms and domains against.
//!
//! # Why this is a value and not a set of labels
//!
//! A `REGION` on a member row is a free-text label today, and an acknowledge
//! level counts it — but nothing can VALIDATE a layout, because the cluster
//! does not know which rows form a realm or how many failure domains a realm
//! spans. A stated topology is that knowledge as catalog data: the realm and
//! domain structure, and the survival claim the operator is arranging against.
//!
//! # What is checked here and what is checked where the membership is readable
//!
//! [`Topology::stated`] checks the STRUCTURE — the set holds together on its
//! own: at least one realm, no realm named twice, no domain named twice within
//! its realm, no empty names. The relations that need the cluster's actual
//! membership — whether the voters outside a realm still hold a majority
//! (the claim arithmetic), whether copies span realms, whether acknowledge
//! levels fit — are checked where the replicas and namespaces are readable:
//! the catalog's write of the topology row. A check that needs the membership
//! cannot live in a value the parser hands over, and a value that silently
//! stored an impossible structure would make the catalog check a second,
//! surprising refusal.
//!
//! # What the claim is, and is not
//!
//! `survives_realm_loss` is a CLAIM the scheme makes, validated against the
//! membership at write time and reported live by the read-back. It is not a
//! runtime gate: the topology guides placement and acknowledges, it never
//! decides a write. A second authority over leadership is how the split-brain
//! the lease prevents arrives through the mechanism meant to prevent it — the
//! same reasoning that keeps the failover periods a statement and not a file.

use crate::error::{Error, Result};

/// One realm of a declared topology — a datacenter-sized failure unit — with
/// the domains (racks, availability zones) it spans.
///
/// Built only through [`Realm::stated`], for the reason [`Topology`] is: the
/// fields are private so a struct literal cannot skip the checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realm {
    /// The realm's name, as written — the same text a member row names with
    /// `REGION '…'`.
    name: String,
    /// The domains within it, as written. Empty when the declaration named
    /// none: a realm without domains is the smallest failure unit it models.
    domains: Vec<String>,
}

impl Realm {
    /// A realm as an operator stated it, refused if its names do not hold
    /// together.
    ///
    /// # Errors
    ///
    /// [`Error::TopologyEmptyName`] when the realm or a domain is named with
    /// the empty string — a realm that reads as nothing collides with every
    /// other realm that does, and a row's `REGION ''` labels nothing.
    ///
    /// [`Error::TopologyDomainNamedTwice`] when two domains of this realm
    /// carry one name: two units a scheme cannot tell apart are one unit
    /// wearing two names, and the survival arithmetic would count them as
    /// the separation they do not provide.
    pub fn stated(name: String, mut domains: Vec<String>) -> Result<Self> {
        if name.is_empty() {
            return Err(Error::TopologyEmptyName {
                what: "a realm",
                realm: name,
            });
        }
        for domain in &domains {
            if domain.is_empty() {
                return Err(Error::TopologyEmptyName {
                    what: "a domain of this realm",
                    realm: name,
                });
            }
        }
        domains.sort();
        if let Some(domain) = domains
            .iter()
            .zip(domains.iter().skip(1))
            .find_map(|(earlier, later)| (earlier == later).then(|| (*later).clone()))
        {
            return Err(Error::TopologyDomainNamedTwice {
                realm: name,
                domain,
            });
        }
        domains.dedup();
        Ok(Self { name, domains })
    }

    /// The realm's name, as written.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The domains within it, in the sorted order the check left them —
    /// the read-back reports a set, and a set has no second order to drift.
    #[must_use]
    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// Whether this realm declares the named domain.
    #[must_use]
    pub fn has_domain(&self, domain: &str) -> bool {
        self.domains.iter().any(|held| held == domain)
    }
}

/// The realms and domains a cluster is arranged over, and the survival claim
/// stated for it.
///
/// Built only through [`Topology::stated`]: the fields are private so a value
/// in hand is one whose structure has been checked, exactly as [`Failover`]
/// is the only way to hold a policy whose periods hold together.
///
/// [`Failover`]: crate::Failover
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// The realms, in the order stated — the read-back shows the arrangement
    /// as written, and the survival verdicts are computed per realm.
    realms: Vec<Realm>,
    /// `SURVIVES REALM LOSS`: the claim that losing any one realm leaves a
    /// cluster that still elects and still holds every acknowledged write.
    /// Absent means the structure is declared with no claim made — the
    /// read-back reports the layout and no survival arithmetic.
    survives_realm_loss: bool,
}

impl Topology {
    /// A topology as an operator stated it, refused if the structure does not
    /// hold together.
    ///
    /// # Errors
    ///
    /// [`Error::TopologyNeedsARealm`] when no realm is stated — a topology
    /// with no realms declares nothing, and storing it would make the
    /// read-back answer *arranged over* with an empty page.
    ///
    /// [`Error::TopologyRealmNamedTwice`] when two realms carry one name: the
    /// survival arithmetic counts realms by name, and two realms it cannot
    /// tell apart are counted as the separation they do not provide.
    ///
    /// [`Error::TopologyEmptyName`] / [`Error::TopologyDomainNamedTwice`] from
    /// each realm's own check, carried unchanged.
    pub fn stated(realms: Vec<Realm>, survives_realm_loss: bool) -> Result<Self> {
        if realms.is_empty() {
            return Err(Error::TopologyNeedsARealm);
        }
        for (index, later) in realms.iter().enumerate().skip(1) {
            if let Some(earlier) = realms[..index]
                .iter()
                .find(|earlier| earlier.name() == later.name())
            {
                return Err(Error::TopologyRealmNamedTwice {
                    realm: earlier.name().to_owned(),
                });
            }
        }
        Ok(Self {
            realms,
            survives_realm_loss,
        })
    }

    /// The realms, in the order stated.
    #[must_use]
    pub fn realms(&self) -> &[Realm] {
        &self.realms
    }

    /// The realm of the given name, when the topology declares one.
    #[must_use]
    pub fn realm(&self, name: &str) -> Option<&Realm> {
        self.realms.iter().find(|realm| realm.name() == name)
    }

    /// Whether the scheme claims a realm's loss is survived.
    #[must_use]
    pub const fn survives_realm_loss(&self) -> bool {
        self.survives_realm_loss
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn realm(name: &str, domains: &[&str]) -> Realm {
        Realm::stated(
            name.to_owned(),
            domains.iter().map(|domain| (*domain).to_owned()).collect(),
        )
        .expect("a well-formed realm")
    }

    #[test]
    fn a_stated_topology_reads_back_exactly_what_was_stated() {
        let topology = Topology::stated(
            vec![realm("dc-a", &["az-1", "az-2"]), realm("dc-b", &[])],
            true,
        )
        .expect("a topology inside every structural relation is accepted");

        assert_eq!(topology.realms().len(), 2);
        assert_eq!(topology.realms()[0].name(), "dc-a");
        assert_eq!(topology.realms()[0].domains(), ["az-1", "az-2"]);
        assert!(topology.realms()[1].domains().is_empty());
        assert!(topology.survives_realm_loss());
        assert!(topology.realm("dc-b").is_some());
        assert!(topology.realm("dc-c").is_none());
    }

    #[test]
    fn a_structure_without_a_claim_is_accepted_and_says_so() {
        // Silence is a third state (a namespace that never said is not one
        // that declined): a topology that declares the layout and no claim
        // must read back as exactly that, not as the weakest claim and not
        // as the strongest.
        let topology = Topology::stated(vec![realm("dc-a", &[])], false)
            .expect("structure without a claim is a real topology");

        assert!(!topology.survives_realm_loss());
    }

    #[test]
    fn a_topology_with_no_realms_is_refused() {
        let refused =
            Topology::stated(vec![], true).expect_err("an empty topology declares nothing");

        assert!(matches!(refused, Error::TopologyNeedsARealm));
    }

    #[test]
    fn two_realms_with_one_name_are_refused_and_named() {
        let refused = Topology::stated(vec![realm("dc-a", &[]), realm("dc-a", &[])], false)
            .expect_err("two realms the arithmetic cannot tell apart are one realm");

        match refused {
            Error::TopologyRealmNamedTwice { realm } => assert_eq!(realm, "dc-a"),
            other => panic!("expected the duplicate-realm refusal, got {other}"),
        }
    }

    #[test]
    fn two_domains_with_one_name_are_refused_with_the_realm() {
        let refused = Realm::stated(
            "dc-a".to_owned(),
            vec!["az-1".to_owned(), "az-1".to_owned()],
        )
        .expect_err("two domains counted as separation they do not provide");

        match refused {
            Error::TopologyDomainNamedTwice { realm, domain } => {
                assert_eq!(realm, "dc-a");
                assert_eq!(domain, "az-1");
            }
            other => panic!("expected the duplicate-domain refusal, got {other}"),
        }
    }

    #[test]
    fn an_empty_realm_name_is_refused() {
        let refused = Realm::stated(String::new(), vec![])
            .expect_err("a realm that reads as nothing labels nothing");

        match refused {
            Error::TopologyEmptyName { what, realm } => {
                assert_eq!(what, "a realm");
                assert!(realm.is_empty());
            }
            other => panic!("expected the empty-name refusal, got {other}"),
        }
    }
}
