//! Which seams each kind of node sits on.
//!
//! This lives beside the node definition rather than in whoever asks, because two things ask and
//! they have to agree. `EXPLAIN` reads it to decide whether a line gets the reference marker, and
//! the executor reads it to decide what the metrics document says ran. A plan printed as all
//! reference and the same plan measured as something else would be a pair of answers where only one
//! can be right, and the one somebody quotes would be whichever they happened to look at.

use rudb_seam::SeamId;

use crate::node::Node;

/// The seams an operator's answer depends on.
///
/// The operator's own seams rather than every seam a query touches. A scan sits on how a column is
/// carried and on when a column is read, a join sits on how its build side is made probeable and on
/// the three hash seams under that, and a limit sits on nothing at all because there is one way to
/// count to ten.
#[must_use]
pub fn seams_of(node: &Node) -> &'static [SeamId] {
    const HASHED: &[SeamId] = &[SeamId::HashKey, SeamId::HashFunction, SeamId::HashTable];
    match node {
        Node::Get { .. } => &[SeamId::VectorForm, SeamId::ScanMaterialisation],
        Node::Filter { .. } => &[
            SeamId::ExprEval,
            SeamId::KernelCompare,
            SeamId::KernelFilter,
            SeamId::ChunkCompaction,
        ],
        Node::Project { .. } => &[SeamId::ExprEval],
        Node::Aggregate { .. } => &[
            SeamId::HashKey,
            SeamId::HashFunction,
            SeamId::HashTable,
            SeamId::AggState,
            SeamId::AggParallel,
        ],
        Node::Window { .. } => &[SeamId::Sort],
        Node::Distinct { .. } | Node::SetOp { .. } => HASHED,
        Node::Sort { .. } => &[SeamId::Sort],
        Node::TopN { .. } => &[SeamId::TopK],
        Node::Join { .. } => {
            &[SeamId::JoinBuild, SeamId::JoinFilter, SeamId::HashKey, SeamId::HashFunction]
        }
        Node::Dummy
        | Node::Values { .. }
        | Node::TableFunction { .. }
        | Node::LateralFunction { .. }
        | Node::Fetch { .. }
        | Node::TableFetch { .. }
        | Node::Limit { .. }
        | Node::LimitPercent { .. }
        | Node::CrossProduct { .. }
        | Node::MaterializedCte { .. }
        | Node::CteScan { .. }
        | Node::DependentJoin { .. } => &[],
    }
}

#[cfg(test)]
mod tests {
    use rudb_seam::SeamId;

    use super::seams_of;
    use crate::node::{Bound, Node};

    #[test]
    fn an_operator_with_one_way_to_do_its_job_sits_on_no_seam() {
        let limit = Node::Limit { input: 0, count: Bound::Rows(10), offset: Bound::Rows(0) };
        assert!(seams_of(&limit).is_empty());
        assert!(seams_of(&Node::Dummy).is_empty());
    }

    #[test]
    fn a_filter_sits_on_the_seam_a_filter_actually_chooses_from() {
        // The chunk compaction seam is the one with a registry, so this is the entry that decides
        // what a filter's row in the metrics document says.
        let seams = seams_of(&Node::Filter { input: 0, predicate: 0 });
        assert!(seams.contains(&SeamId::ChunkCompaction), "{seams:?}");
    }
}
