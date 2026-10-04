use super::*;

impl Held {
    /// Hold nothing.
    ///
    /// The default, and it refuses everything — which is what makes "no
    /// authority matched" the structural answer rather than a rule somebody
    /// remembered to write.
    #[must_use]
    pub fn nothing() -> Self {
        Self(BTreeSet::new())
    }

    /// Hold exactly these.
    #[must_use]
    pub fn of(authorities: impl IntoIterator<Item = Authority>) -> Self {
        Self(authorities.into_iter().collect())
    }

    /// Every kind that can be held at one reach — the shape an owner of
    /// something has.
    ///
    /// "Every kind" is filtered rather than literal, and the filter is the whole
    /// point of putting it here. This is not a statement anybody types: it is
    /// the bundle [`Self::from_role`] hands an owner, so a kind that must not
    /// reach a namespace would arrive at every namespace owner in the store by a
    /// road with no author. One filter, and the role follows it for free.
    #[must_use]
    pub fn every_kind_at(reach: Reach) -> Self {
        Self::of(
            Kind::ALL
                .iter()
                .filter(|kind| kind.may_be_held_at(reach))
                .map(|kind| Authority::new(*kind, reach)),
        )
    }

    /// Add one.
    pub fn add(&mut self, authority: Authority) {
        self.0.insert(authority);
    }

    /// Take one away.
    ///
    /// Only the authority named, never one that contains it: revoking `write` at
    /// a database does not silently narrow a `write` held over the whole store,
    /// because a revocation that rewrites a *different* grant is one nobody can
    /// predict the effect of.
    pub fn remove(&mut self, authority: &Authority) -> bool {
        self.0.remove(authority)
    }

    /// Whether anything held answers a demand for `kind` at `reach`.
    #[must_use]
    pub fn permits(&self, kind: Kind, reach: Reach) -> bool {
        self.0.iter().any(|held| held.permits(kind, reach))
    }

    /// Whether anything at all is held at `reach` or above it.
    ///
    /// The question `USE` asks: selecting a namespace should not require the
    /// authority to *read* it — that would stop a govern-only administrator
    /// selecting the namespace they administer — but requiring nothing at all
    /// would make the statement an existence oracle over every namespace.
    #[must_use]
    pub fn anything_at(&self, reach: Reach) -> bool {
        self.0.iter().any(|held| held.reach.contains(reach))
    }

    /// Whether this container is on the path between the store and something
    /// held — at it, above it, or inside it.
    ///
    /// # Why `USE` needs both directions and [`Self::anything_at`] does not
    ///
    /// Containment runs downward: a namespace contains its databases and a
    /// database contains nothing above it. That is right for a *demand*, which
    /// must be answered **at** the container the statement reaches.
    ///
    /// Selecting is not a demand. A user scoped to `prod.shop` has to say
    /// `USE NAMESPACE prod` before they can say `USE DATABASE shop`, so the
    /// namespace is a step on the way to the only thing they hold — and asking
    /// downward containment alone refuses them their own database. Measured, not
    /// predicted: every database-scoped user in the suite was locked out of the
    /// store by exactly that.
    ///
    /// The upward direction is not a widening of authority. It permits *naming*
    /// a container, and every statement that then acts inside it is asked the
    /// ordinary question at the ordinary reach. What it rules out is the case it
    /// exists for: naming a container the caller has no business in at all, and
    /// learning from the refusal whether it is there.
    #[must_use]
    pub fn touches(&self, reach: Reach) -> bool {
        self.0
            .iter()
            .any(|held| held.reach.contains(reach) || reach.contains(held.reach))
    }

    /// What is held, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = Authority> + '_ {
        self.0.iter().copied()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The authorities a declared role stood for, at the reach it applied to.
    ///
    /// # This mapping keeps a bundle the new rule forbids, and that is correct
    ///
    /// `Editor` becomes `{read, write, manage}` — precisely the combination the
    /// separation exists to make refusable. It is not a mistake to be tidied.
    /// The rule governs what can now be **said**; every editor that already
    /// exists was declared under a promise that they may define structure, and
    /// narrowing them on upgrade is an outage delivered as a migration. What the
    /// change buys is that nobody has to accept the bundle any more.
    ///
    /// # An owner gained [`Kind::Replicate`] when the sixth kind arrived
    ///
    /// The mirror of the paragraph above — widening an existing principal on
    /// upgrade is an escalation delivered as a migration — so it was decided
    /// rather than inherited from [`Self::every_kind_at`].
    ///
    /// It stands, for two reasons and a residue. An owner **at the store**
    /// already holds `read` and `operate` there, which together are `BACKUP`:
    /// every record and every definition, in one file. The log discloses nothing
    /// to them that they could not already take, so this widens what they may
    /// *do* and not what they may *see*. And excluding it would make the kind
    /// unreachable rather than merely explicit: nobody hands out what they do
    /// not hold, so a store whose users were all declared by role could never
    /// grant `replicate` to anybody, including to itself.
    ///
    /// The residue was an owner of one **namespace**, who gained an authority
    /// that authorised nothing while the only subscription that could be served
    /// was the whole store's — and which must not, once a selective stream
    /// exists, carry the identity class with it. **That residue is now paid.**
    /// [`Kind::may_be_held_at`] makes `replicate` a thing held over the store or
    /// not at all, [`Self::every_kind_at`] filters by it, and this mapping
    /// inherits the narrowing without an edit: an owner at the store still holds
    /// every kind, an owner of a namespace no longer holds that one.
    ///
    /// Note which direction that moved. Narrowing a role on upgrade is the
    /// outage this doc warns about two paragraphs above, and this is one — a
    /// namespace owner loses an authority they were declared with. It is taken
    /// anyway because what they lose is an authority that never authorised
    /// anything, and what it buys is that the identity class can travel to every
    /// follower without a tenant being able to ask for it.
    #[must_use]
    pub fn from_role(role: Role, reach: Reach) -> Self {
        match role {
            Role::Viewer => Self::of([Authority::new(Kind::Read, reach)]),
            Role::Editor => Self::of([
                Authority::new(Kind::Read, reach),
                Authority::new(Kind::Write, reach),
                Authority::new(Kind::Manage, reach),
            ]),
            Role::Owner => Self::every_kind_at(reach),
        }
    }

    /// The widest role whose bundle this set contains, if any role does.
    ///
    /// The inverse of [`Self::from_role`], and it is a *summary* rather than a
    /// round trip: a set is written to the catalog alongside the role it can be
    /// described as, so that a binary predating the set field reads a role and
    /// under-grants rather than misreading. Widest-that-fits, never
    /// nearest — a role wider than the set would grant an older binary
    /// something the user does not hold, which is the one direction this must
    /// never fail in.
    ///
    /// `None` is the honest answer for the sets a ladder could never express —
    /// `manage` at a namespace without `read` is the case the whole model
    /// exists for. It is written as an **absent** role, which an older binary
    /// refuses to read rather than guessing at.
    #[must_use]
    pub fn role_within(&self, reach: Reach) -> Option<Role> {
        Role::ALL
            .iter()
            .rev()
            .copied()
            .find(|role| Self::from_role(*role, reach).0.is_subset(&self.0))
    }

    /// The set, as it is written to the catalog.
    ///
    /// One string per authority — `"read@store"`, `"write@3"`, `"manage@3.7"` —
    /// rather than a nested object per entry. The set is small, the grammar is
    /// closed, and a flat list of short strings is a form a person reading a
    /// catalog dump can check by eye.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|held| Value::from(written(*held).as_str()))
                .collect(),
        )
    }

    /// Read the set back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the value is not an array of
    /// authorities this binary knows. An unknown kind is corruption rather than
    /// a bad request — it was written by something that knew a kind this binary
    /// does not, and guessing would grant or refuse the wrong thing.
    pub fn from_value(value: &Value) -> Result<Self> {
        let malformed = |found: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field: "authorities",
            found,
        };
        let Value::Array(entries) = value else {
            return Err(malformed(value.type_name()));
        };
        let mut held = BTreeSet::new();
        for entry in entries {
            let Value::String(text) = entry else {
                return Err(malformed(entry.type_name()));
            };
            held.insert(read(text).ok_or_else(|| malformed("an unknown authority"))?);
        }
        Ok(Self(held))
    }
}
