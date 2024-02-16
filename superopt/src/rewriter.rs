//! Rewriter for the SuperOptimizer.

use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use quizx::vec_graph::{EType, VType};
use quizx::{
    flow::causal::CausalFlow,
    graph::GraphLike,
    portmatching::{CausalMatcher, CausalPattern, PatternID},
    vec_graph::V,
};

use crate::rewrite_sets::RuleSide;
use crate::{
    cost::CostDelta,
    rewrite_sets::{RewriteRhs, RewriteSet},
};

pub trait Rewriter {
    type Rewrite;

    /// Get the rewrites that can be applied to the graph.
    fn get_rewrites(&self, graph: &impl GraphLike) -> Vec<Self::Rewrite>;

    /// Apply the rewrites to the graph.
    fn apply_rewrite<G: GraphLike>(&self, rewrite: Self::Rewrite, graph: &G) -> RewriteResult<G>;
}

pub struct RewriteResult<G> {
    pub graph: G,
    pub cost_delta: CostDelta,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RhsIdx(usize);

/// A rewriter that applies causal flow preserving rewrites.
///
/// The set of possible rewrites are given as a list of `RewriteSet`s.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CausalRewriter<G: GraphLike> {
    matcher: CausalMatcher<G>,
    lhs_to_rhs: HashMap<PatternID, RhsIdx>,
    all_rhs: Vec<Vec<RewriteRhs<G>>>,
}

pub struct Rewrite<G> {
    /// The nodes matching the LHS boundary in the matched graph.
    lhs_boundary: Vec<V>,
    /// The nodes matching the RHS boundary in `rhs`.
    rhs_boundary: Vec<V>,
    /// The internal nodes of the LHS in the matched graph.
    lhs_internal: HashSet<V>,
    /// The replacement graph.
    rhs: G,
}

impl<G: GraphLike> Rewriter for CausalRewriter<G> {
    type Rewrite = Rewrite<G>;

    fn get_rewrites(&self, graph: &impl GraphLike) -> Vec<Self::Rewrite> {
        let flow = CausalFlow::from_graph(graph).expect("no causal flow");
        self.matcher
            .find_matches(graph, &flow)
            .flat_map(|m| {
                self.get_rhs(m.pattern_id).iter().map(move |rhs| {
                    let lhs_boundary = m.boundary.clone();
                    let lhs_internal = m.internal.clone();
                    let rhs_boundary = rhs.boundary().collect_vec();
                    let rhs = rhs.graph().clone();
                    assert_eq!(lhs_boundary.len(), rhs_boundary.len());
                    Rewrite {
                        lhs_boundary,
                        rhs_boundary,
                        lhs_internal,
                        rhs,
                    }
                })
            })
            .collect()
    }

    fn apply_rewrite<H: GraphLike>(&self, rewrite: Self::Rewrite, graph: &H) -> RewriteResult<H> {
        let mut g = graph.clone();
        let mut new_r_names: HashMap<V, V> = HashMap::new();

        // Remove the internal nodes of the LHS.
        for v in rewrite.lhs_internal {
            g.remove_vertex(v);
        }

        // Replace the LHS boundary nodes with the RHS's.
        for (&l, &r) in rewrite.lhs_boundary.iter().zip(rewrite.rhs_boundary.iter()) {
            new_r_names.insert(r, l);
            g.set_phase(l, rewrite.rhs.phase(r));
            g.set_vertex_type(l, rewrite.rhs.vertex_type(r));
        }

        // Insert the internal nodes of the RHS.
        for r in rewrite.rhs.vertices() {
            if new_r_names.contains_key(&r) {
                // It was already added as a boundary node.
                continue;
            }

            let vtype = rewrite.rhs.vertex_type(r);
            if vtype == VType::B {
                continue;
            }

            let l = g.add_vertex_with_phase(vtype, rewrite.rhs.phase(r));
            new_r_names.insert(r, l);
        }

        // Reconnect the edges.
        for (u, v, ty) in rewrite.rhs.edges() {
            let (Some(&u), Some(&v)) = (new_r_names.get(&u), new_r_names.get(&v)) else {
                // Ignore the boundary edges.
                continue;
            };
            assert_eq!(ty, EType::H);
            g.add_edge_smart(u, v, ty);
        }

        RewriteResult {
            graph: g,
            cost_delta: CostDelta::default(),
        }
    }
}

impl<G: GraphLike + Clone> CausalRewriter<G> {
    fn get_rhs(&self, lhs_idx: PatternID) -> &[RewriteRhs<G>] {
        let idx = &self.lhs_to_rhs[&lhs_idx];
        &self.all_rhs[idx.0]
    }

    pub fn from_rewrite_rules(rules: impl IntoIterator<Item = RewriteSet<G>>) -> Self {
        let mut patterns = Vec::new();
        let mut map_to_rhs = HashMap::new();
        let mut all_rhs = Vec::new();
        for rw_set in rules {
            let rhs_idx = RhsIdx(all_rhs.len());
            all_rhs.push(rw_set.rhss().to_owned());
            let boundary = rw_set.lhs().boundary().collect_vec();
            for (inputs, outputs) in rw_set.lhs().ios() {
                let mut p = rw_set.lhs().graph().clone();
                p.set_inputs(inputs);
                p.set_outputs(outputs);
                let flow = CausalFlow::from_graph(&p).expect("invalid causal flow in pattern");
                patterns.push(CausalPattern::new(p, flow, boundary.clone()));
                map_to_rhs.insert(PatternID(patterns.len() - 1), rhs_idx);
            }
        }
        CausalRewriter {
            matcher: CausalMatcher::from_patterns(patterns),
            lhs_to_rhs: map_to_rhs,
            all_rhs,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::cost::{CostMetric, TwoQubitGateCount};

    use super::*;
    use quizx::vec_graph::Graph;
    use rstest::{fixture, rstest};

    const TEST_SET: &str = include_str!("../../test_files/rewrites-2qb-lc.json");

    #[fixture]
    fn rewrite_set_2qb_lc() -> Vec<RewriteSet<Graph>> {
        serde_json::from_str(TEST_SET).unwrap()
    }

    /// Makes a simple graph, with 2 inputs and 2 outputs.
    #[fixture]
    fn simple_graph() -> (Graph, Vec<V>) {
        let mut g = Graph::new();
        let vs = vec![
            g.add_vertex(VType::B),
            g.add_vertex(VType::B),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::Z),
            g.add_vertex(VType::B),
            g.add_vertex(VType::B),
        ];

        g.set_inputs(vec![vs[0], vs[1]]);
        g.set_outputs(vec![vs[10], vs[11]]);

        g.add_edge_with_type(vs[0], vs[2], EType::N);
        g.add_edge_with_type(vs[1], vs[3], EType::N);

        g.add_edge_with_type(vs[2], vs[4], EType::H);
        g.add_edge_with_type(vs[3], vs[5], EType::H);
        g.add_edge_with_type(vs[2], vs[3], EType::H);

        g.add_edge_with_type(vs[4], vs[6], EType::H);
        g.add_edge_with_type(vs[5], vs[7], EType::H);

        g.add_edge_with_type(vs[6], vs[8], EType::H);
        g.add_edge_with_type(vs[7], vs[9], EType::H);
        g.add_edge_with_type(vs[6], vs[7], EType::H);

        g.add_edge_with_type(vs[8], vs[10], EType::N);
        g.add_edge_with_type(vs[9], vs[11], EType::N);

        (g, vs)
    }

    #[rstest]
    fn test_match_apply(
        rewrite_set_2qb_lc: Vec<RewriteSet<Graph>>,
        simple_graph: (Graph, Vec<V>),
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rewriter = CausalRewriter::from_rewrite_rules(rewrite_set_2qb_lc);
        let (g, _) = simple_graph;
        let cost_metric = TwoQubitGateCount::new();
        let graph_cost = cost_metric.cost(&g);

        let rewrites = rewriter.get_rewrites(&g);

        println!("Orig cost {graph_cost}");
        for rw in rewrites {
            let r = rewriter.apply_rewrite(rw, &g);
            let new_cost = cost_metric.cost(&r.graph);

            println!("New cost {new_cost}");
            assert_eq!(graph_cost.saturating_add_signed(r.cost_delta), new_cost);
        }

        Ok(())
    }
}
