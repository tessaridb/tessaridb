use super::*;

/// How many password verifications this process will run at once.
///
/// Unit: concurrent verifications.
///
/// # What this bounds, and why it is not the connection ceiling
///
/// A password hash is deliberately expensive — [`PASSWORD_HASH_MEMORY_KIB`] of
/// memory and tens of milliseconds of CPU, by design, because that is what makes
/// an offline crack of a stolen catalog slow. Online and unbounded, the same
/// property makes an **amplifier**: one TCP write costs the attacker nothing and
/// costs this node nineteen mebibytes, and **no attempt has to be valid**, so no
/// credential is needed to spend the memory.
///
/// [`MAX_CONNECTIONS`] does not close it. Four hundred connections all
/// presenting a credential is several gigabytes, which is the multiplication
/// rather than the bound.
///
/// # Why twenty-four, and why a refusal
///
/// The number is derived rather than chosen: the budget this node is willing to
/// lose to authentication is **512 MiB** — enough to matter, small enough to
/// leave the store's cache standing — and one verification costs
/// [`PASSWORD_HASH_MEMORY_KIB`], so the budget buys twenty-six and twenty-four
/// is that with room left. A test asserts the product against the budget, so the
/// two cannot drift apart quietly.
///
/// It was eight first, which was a number nobody derived, and being a third of
/// its own stated budget it refused sign-ins a database has no business
/// refusing: nine clients authenticating at once is an ordinary Tuesday, not an
/// attack. A bound set below ordinary use is not a security control, it is an
/// outage that only fires under load.
///
/// Refused rather than queued, for the reason the door itself gives: a queue
/// moves the unbounded growth instead of removing it, and a client told to come
/// back can, while a client parked in a queue cannot even tell that it is
/// waiting.
///
/// The cost is real and belongs here rather than in a surprise: a fleet
/// reconnecting all at once signs in twenty-four at a time and the rest are
/// refused and retry. That is the trade a bound is.
pub const MAX_SIGN_IN_VERIFICATIONS: usize = 24;

/// How long a session token stays good for.
///
/// Unit: seconds.
///
/// Twelve hours, which covers a working day without covering the night after
/// it. The number is a trade between two costs that pull opposite ways: a short
/// life sends every client back through the memory-hard sign-in
/// [`MAX_SIGN_IN_VERIFICATIONS`] exists to bound, and a long one widens the
/// window in which a token copied off the wire is still worth having.
///
/// It bounds the window and not the damage. What actually revokes a token is
/// the user record changing under it — a rotated password, a corrected role, a
/// removal — and that takes effect on the very next request rather than at
/// expiry. This constant is what covers the case nobody noticed and so nobody
/// revoked.
pub const SESSION_TOKEN_SECONDS: u64 = 12 * 60 * 60;

/// How long an unseal lasts before the store seals itself again (ADR-0092 D4).
///
/// Unit: seconds. Overridden per node by `--unseal-for` / `TESSARIDB_UNSEAL_FOR`.
///
/// Ten minutes, the owner's figure: long enough to unseal, do the work that
/// needed it and leave, short enough that a store nobody remembered to seal is
/// not left open for the rest of the process's life. Measured from the unseal
/// and never renewed by use — a busy node that renewed on every read would never
/// seal, which is the opposite of what the period is for.
pub const UNSEAL_SECONDS: u64 = 10 * 60;

/// How many session tokens one node will hold at once.
///
/// Unit: tokens.
///
/// A bound on memory a caller who *does* hold a valid credential could
/// otherwise grow without limit: signing in successfully is not throttled — only
/// failing is — so nothing else stands between one account and an unbounded
/// table.
///
/// Ten per store-call slot ([`MAX_STORE_CALLS`]) — the connection bound this
/// was sized against before the two were separated — because a client that
/// reconnects gets a new connection and may reasonably still hold its old
/// token. At roughly two hundred bytes an entry the whole table is under a
/// mebibyte, which is the point: it is cheap enough that the bound can be
/// generous and still be a bound.
///
/// Reaching it **refuses to issue** rather than evicting somebody else's live
/// token. Eviction would make minting tokens a way to sign other people out.
pub const MAX_SESSION_TOKENS: usize = MAX_STORE_CALLS * 10;

/// How many times one identity may fail to sign in before it is made to wait.
///
/// Unit: consecutive failures.
///
/// Three, because a person mistyping a password twice is ordinary and a third
/// consecutive miss is where a human stops guessing and goes to look the
/// password up. A success clears the count, so the allowance is renewed by
/// getting it right rather than by waiting.
pub const FREE_SIGN_IN_FAILURES: u32 = 3;

/// How long an identity waits after its first throttled failure, doubling with
/// each further one.
///
/// Unit: milliseconds.
///
/// # Why doubling, and why this is a delay rather than a lockout
///
/// A fixed delay is a fixed guess rate, and a fixed guess rate is still a rate:
/// an attacker willing to spend a week gets a week's worth of guesses out of it.
/// Doubling makes the total attempts available in any window logarithmic instead
/// of linear, which is the difference between slowing an attack and ending it.
///
/// It is not a lockout, and that is deliberate. A permanent lockout hands an
/// attacker a denial of service against any account whose name they know — they
/// need no password to fail three times. A delay that decays to nothing when the
/// attack stops costs a real user a pause and costs an attacker the attack.
///
/// A quarter of a second is below the point where a person retrying by hand
/// notices, and it is already five times the cost of the verification it is
/// standing in front of.
pub const SIGN_IN_BACKOFF_MILLIS: u64 = 250;

/// The longest an identity waits between sign-in attempts, however many it has
/// missed.
///
/// Unit: milliseconds.
///
/// Doubling without a ceiling reaches days, and a real user who mistyped a
/// password six times would be locked out in everything but name — which is
/// exactly what [`SIGN_IN_BACKOFF_MILLIS`] argues against. Thirty seconds is long
/// enough that a sustained attack is measured in attempts per hour and short
/// enough that a person who went to find their password comes back to a store
/// that will talk to them.
pub const SIGN_IN_BACKOFF_CEILING_MILLIS: u64 = 30_000;

/// How much memory one password hash is made to cost.
///
/// Unit: kibibytes.
///
/// # Why this number is written down at all
///
/// It is Argon2id's `m` parameter, and it was previously whatever the hashing
/// crate's `Default` said. That is the thing **LR-DB-004** forbids: a default
/// that was never read is not evidence, and a routine dependency upgrade that
/// moved it would change this store's password-hashing posture with nothing in
/// the repository, the decision records or the tests showing that it had
/// happened. Read from `argon2` 0.5.3 and recorded in ADR-0043, along with the
/// two below.
///
/// Nineteen mebibytes is the second of OWASP's Argon2id options — the one paired
/// with two passes — and memory is the parameter that actually resists a GPU,
/// because a GPU has thousands of cores and not thousands of memory channels.
///
/// It is also, directly, the amplification factor
/// [`MAX_SIGN_IN_VERIFICATIONS`] exists to bound: raising it makes a stolen
/// catalog harder to crack **and** makes an unauthenticated attempt more
/// expensive to serve, so the two constants are read together or neither is
/// understood.
pub const PASSWORD_HASH_MEMORY_KIB: u32 = 19_456;

/// How many passes one password hash is made to take over that memory.
///
/// Unit: passes.
///
/// Argon2id's `t`. Two, which is what OWASP pairs with nineteen mebibytes: at a
/// fixed cost budget, memory buys more resistance than iterations do, so the
/// passes are the parameter kept low. Recorded with the crate version in
/// ADR-0043 for the reason [`PASSWORD_HASH_MEMORY_KIB`] gives.
pub const PASSWORD_HASH_PASSES: u32 = 2;

/// How many lanes one password hash is spread across.
///
/// Unit: lanes.
///
/// Argon2id's `p`. One, so a verification is one thread's work. Parallelism here
/// would divide the wall-clock cost of a single hash by spending more of the
/// machine on it, which on a serving node is the wrong direction twice over: the
/// point of the cost is that it is paid, and a node under an authentication
/// flood would multiply its own load by the lane count. Recorded in ADR-0043.
pub const PASSWORD_HASH_LANES: u32 = 1;
