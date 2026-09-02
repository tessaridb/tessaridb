//! Adjacency keys — a node's neighbours, held beside the node.
//!
//! ```text
//! <0x14> <ns:u32> <db:u32> <graph:u32> <node-table:u32> <node-id>
//!        <edge-kind:u32> <dir:u8> <neighbour-table:u32> <neighbour-id>
//! ```
//!
//! # Why this layout and not another
//!
//! Every component sits where it does because a query depends on it being
//! there, and the order is the whole engine difference between this and an edge
//! table reached through an index.
//!
//! - **`<ns><db>` first**, as every other key kind in this store, so a tenancy
//!   stays one contiguous range and dropping a database stays one range delete.
//! - **`<graph>` above the node**, so the whole structure is one prefix and
//!   `DROP GRAPH` is a range delete rather than a scan. This is the key-layout
//!   consequence of a graph being an object the store holds.
//! - **`<node-table><node-id>` together**, so a node's entire adjacency — every
//!   edge kind, both directions — is one contiguous range. *"Everything touching
//!   this node"* is one scan.
//! - **`<edge-kind>` next**, because the common question is *"this node's
//!   `works_at` edges"*. Putting direction above the edge kind would make that
//!   question two ranges instead of one.
//! - **`<dir>` after the edge kind**, so both directions of one kind stay
//!   adjacent: an undirected neighbourhood is still one scan and a directed one
//!   is a narrower prefix of it.
//! - **the neighbour last**, which makes the entry unique and makes *"is A
//!   joined to B by `works_at`"* a point read — the same reason a secondary
//!   index carries the primary key last.
//!
//! # Both directions are written, and that is a decision
//!
//! Each edge writes **two** entries. It doubles the adjacency write cost, and it
//! is what makes a reverse walk a range read instead of a full scan. A store
//! that wrote only the forward entry would answer *"who does Alice follow"*
//! cheaply and *"who follows Alice"* by reading everything — and the second is
//! the question a graph is actually asked.
//!
//! # No length prefixes
//!
//! A length prefix sorts `"b"` before `"aa"`, which would order a node's
//! neighbours in a way nothing expects. Every component here is either fixed
//! width or self-delimiting, which is what makes a bounded prefix scan exact
//! rather than approximate.

use tessari_kv::{Key, Value};
use tessari_types::{DatabaseId, EdgeKindId, GraphId, NamespaceId, RecordId, TableId};

use crate::error::{Error, Result};
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};

/// Which way an adjacency entry points.
///
/// One byte, and the two values are adjacent so that both directions of one edge
/// kind form a single range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// From this node towards the neighbour.
    Out,
    /// From the neighbour towards this node.
    In,
}

impl Direction {
    /// The byte written into the key.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Out => 0x00,
            Self::In => 0x01,
        }
    }

    /// The direction a byte names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownDirection`] for any other byte, rather than
    /// defaulting: a mis-decoded direction turns a follower into a followee
    /// without anything being in an error state.
    pub const fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0x00 => Ok(Self::Out),
            0x01 => Ok(Self::In),
            found => Err(Error::UnknownDirection { found }),
        }
    }

    /// The entry that mirrors this one.
    ///
    /// Every edge writes both, and a sweep checks that each has its opposite.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Out => Self::In,
            Self::In => Self::Out,
        }
    }
}

/// One neighbour of one node, under one edge kind, in one direction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdjacencyKey {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The graph the edge kind belongs to.
    pub graph: GraphId,
    /// The table holding the node this entry hangs off.
    pub node_table: TableId,
    /// The node itself.
    pub node: RecordId,
    /// The edge kind joining the two.
    pub edge_kind: EdgeKindId,
    /// Which way the entry points.
    pub direction: Direction,
    /// The table holding the neighbour.
    pub neighbour_table: TableId,
    /// The neighbour.
    pub neighbour: RecordId,
}

impl AdjacencyKey {
    /// Name one entry.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "every component is part of the key grammar; grouping them into \
                  structs would name pairs the layout does not have"
    )]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        graph: GraphId,
        node_table: TableId,
        node: RecordId,
        edge_kind: EdgeKindId,
        direction: Direction,
        neighbour_table: TableId,
        neighbour: RecordId,
    ) -> Self {
        Self {
            namespace,
            database,
            graph,
            node_table,
            node,
            edge_kind,
            direction,
            neighbour_table,
            neighbour,
        }
    }

    /// The entry written on the other endpoint for the same edge.
    ///
    /// The two are written together in one batch, so having the mirror as a
    /// function rather than as something each caller re-derives is what keeps
    /// them from drifting apart.
    #[must_use]
    pub fn mirror(&self) -> Self {
        Self {
            namespace: self.namespace,
            database: self.database,
            graph: self.graph,
            node_table: self.neighbour_table,
            node: self.neighbour.clone(),
            edge_kind: self.edge_kind,
            direction: self.direction.opposite(),
            neighbour_table: self.node_table,
            neighbour: self.node.clone(),
        }
    }

    /// Every entry of one graph — what `DROP GRAPH` deletes as a single range.
    #[must_use]
    pub fn graph_prefix(namespace: NamespaceId, database: DatabaseId, graph: GraphId) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        writer
            .put_u8(KeyKind::Edge.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(graph.get());
        writer.finish()
    }

    /// Everything touching one node, across every edge kind and both directions.
    #[must_use]
    pub fn node_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        graph: GraphId,
        node_table: TableId,
        node: &RecordId,
    ) -> Vec<u8> {
        let mut bytes = Self::graph_prefix(namespace, database, graph);
        let mut writer = KeyWriter::new();
        writer.put_u32(node_table.get());
        record_id::put(&mut writer, node);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }

    /// One node's edges of one kind, in one direction — the prefix a hop scans.
    ///
    /// This is the range read the whole layout exists to make possible: the
    /// neighbours are contiguous, so reaching them costs one scan rather than an
    /// index probe and a random read per neighbour.
    #[must_use]
    pub fn hop_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        graph: GraphId,
        node_table: TableId,
        node: &RecordId,
        edge_kind: EdgeKindId,
        direction: Direction,
    ) -> Vec<u8> {
        let mut bytes = Self::node_prefix(namespace, database, graph, node_table, node);
        let mut writer = KeyWriter::new();
        writer.put_u32(edge_kind.get()).put_u8(direction.tag());
        bytes.extend_from_slice(&writer.finish());
        bytes
    }
}

impl StoreKey for AdjacencyKey {
    type Value = EdgeProperties;

    const KIND: KeyKind = KeyKind::Edge;

    fn encode(&self) -> Key {
        let mut bytes = Self::hop_prefix(
            self.namespace,
            self.database,
            self.graph,
            self.node_table,
            &self.node,
            self.edge_kind,
            self.direction,
        );
        let mut writer = KeyWriter::new();
        writer.put_u32(self.neighbour_table.get());
        record_id::put(&mut writer, &self.neighbour);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let graph = GraphId::new(reader.take_u32()?);
        let node_table = TableId::new(reader.take_u32()?);
        let node = record_id::take(&mut reader)?;
        let edge_kind = EdgeKindId::new(reader.take_u32()?);
        let direction = Direction::from_tag(reader.take_u8()?)?;
        let neighbour_table = TableId::new(reader.take_u32()?);
        let neighbour = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            graph,
            node_table,
            node,
            edge_kind,
            direction,
            neighbour_table,
            neighbour,
        })
    }
}

/// What an edge carries besides its two endpoints.
///
/// # Why the payload is written on both entries rather than pointed at
///
/// The alternative was to store the id of an edge *record* here. That would
/// reintroduce exactly the random read per neighbour this layout exists to
/// remove: a hop would range-read the neighbours and then fetch a record for
/// each one, which is the edge-table shape wearing a new key.
///
/// So the properties are written twice, once per direction. The cost is a
/// duplicated small object per edge. The two copies cannot drift apart through
/// an endpoint change, because an edge is identified by its endpoints and they
/// are immutable; a property update rewrites both entries in one batch, as the
/// write did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EdgeProperties(Vec<u8>);

impl EdgeProperties {
    /// Carry an encoded property object.
    #[must_use]
    pub const fn new(payload: Vec<u8>) -> Self {
        Self(payload)
    }

    /// The encoded bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Whether the edge carries anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl StoreValue for EdgeProperties {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, self.0.len());
        buffer.extend_from_slice(&self.0);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        Ok(Self(payload.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn key(node: i64, kind: u32, direction: Direction, neighbour: i64) -> AdjacencyKey {
        AdjacencyKey::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            GraphId::new(3),
            TableId::new(4),
            RecordId::Int(node),
            EdgeKindId::new(kind),
            direction,
            TableId::new(5),
            RecordId::Int(neighbour),
        )
    }

    #[test]
    fn an_entry_survives_the_round_trip_it_is_stored_through() {
        let original = key(1, 7, Direction::Out, 2);
        let read_back = AdjacencyKey::decode(original.encode().as_slice()).unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn a_text_neighbour_round_trips_beside_an_integer_one() {
        // The four record-id variants carry a leading discriminator, so a node's
        // neighbours group by id kind before sorting within a kind. Asserted
        // rather than assumed, because a decoder that guessed the variant would
        // read a plausible wrong neighbour instead of failing.
        let mut original = key(1, 7, Direction::Out, 2);
        original.neighbour = RecordId::from("alice");
        let read_back = AdjacencyKey::decode(original.encode().as_slice()).unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn entries_sort_by_node_then_kind_then_direction_then_neighbour() {
        // The ordering IS the engine: a hop is a range read only because the
        // neighbours of one node under one kind in one direction are contiguous.
        // Encoded bytes are compared rather than the struct, because it is the
        // bytes the store sorts.
        let ordered = [
            key(1, 7, Direction::Out, 1),
            key(1, 7, Direction::Out, 2),
            key(1, 7, Direction::In, 1),
            key(1, 8, Direction::Out, 1),
            key(2, 7, Direction::Out, 1),
        ];
        for pair in ordered.windows(2) {
            let earlier = pair[0].encode();
            let later = pair[1].encode();
            assert!(
                earlier.as_slice() < later.as_slice(),
                "{:?} should sort before {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_hop_prefix_bounds_exactly_the_entries_it_names() {
        // A prefix that also matched a neighbouring edge kind or the opposite
        // direction would make a hop return edges the caller did not ask for,
        // and nothing would report it as wrong.
        let prefix = AdjacencyKey::hop_prefix(
            NamespaceId::new(1),
            DatabaseId::new(2),
            GraphId::new(3),
            TableId::new(4),
            &RecordId::Int(1),
            EdgeKindId::new(7),
            Direction::Out,
        );
        assert!(
            key(1, 7, Direction::Out, 9)
                .encode()
                .as_slice()
                .starts_with(&prefix)
        );
        assert!(
            !key(1, 7, Direction::In, 9)
                .encode()
                .as_slice()
                .starts_with(&prefix)
        );
        assert!(
            !key(1, 8, Direction::Out, 9)
                .encode()
                .as_slice()
                .starts_with(&prefix)
        );
        assert!(
            !key(2, 7, Direction::Out, 9)
                .encode()
                .as_slice()
                .starts_with(&prefix)
        );
    }

    #[test]
    fn a_node_prefix_covers_both_directions_and_every_kind() {
        let prefix = AdjacencyKey::node_prefix(
            NamespaceId::new(1),
            DatabaseId::new(2),
            GraphId::new(3),
            TableId::new(4),
            &RecordId::Int(1),
        );
        for entry in [
            key(1, 7, Direction::Out, 9),
            key(1, 7, Direction::In, 9),
            key(1, 8, Direction::Out, 9),
        ] {
            assert!(entry.encode().as_slice().starts_with(&prefix), "{entry:?}");
        }
        assert!(
            !key(2, 7, Direction::Out, 9)
                .encode()
                .as_slice()
                .starts_with(&prefix)
        );
    }

    #[test]
    fn the_mirror_of_a_mirror_is_the_entry_itself() {
        // Both entries are written together, so the mirror is a function rather
        // than something each caller re-derives — and an involution is the
        // cheapest statement that it swaps every component it should.
        let original = key(1, 7, Direction::Out, 2);
        assert_eq!(original.mirror().mirror(), original);

        let mirror = original.mirror();
        assert_eq!(mirror.node, original.neighbour);
        assert_eq!(mirror.node_table, original.neighbour_table);
        assert_eq!(mirror.direction, Direction::In);
    }

    #[test]
    fn an_unknown_direction_byte_is_refused_rather_than_defaulted() {
        // A direction that decoded to a default would turn a follower into a
        // followee with nothing in an error state.
        let error = Direction::from_tag(0x02).unwrap_err();
        assert!(
            matches!(error, Error::UnknownDirection { found: 0x02 }),
            "{error}"
        );
    }

    #[test]
    fn properties_round_trip_and_an_edge_without_any_stays_empty() {
        let carried = EdgeProperties::new(vec![1, 2, 3]);
        assert_eq!(
            EdgeProperties::decode(carried.encode().as_slice()).unwrap(),
            carried
        );

        let bare = EdgeProperties::default();
        assert!(bare.is_empty());
        assert!(
            EdgeProperties::decode(bare.encode().as_slice())
                .unwrap()
                .is_empty()
        );
    }
}
