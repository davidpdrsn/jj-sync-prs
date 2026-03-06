use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

#[derive(Debug, Default)]
pub struct Graph {
    nodes: Vec<String>,
    edges: BTreeMap<usize, BTreeSet<usize>>,
}

impl Graph {
    pub fn get_or_insert(&mut self, node: &str) -> usize {
        for (idx, n) in self.nodes.iter().enumerate() {
            if n == node {
                return idx;
            }
        }
        self.nodes.push(node.to_owned());
        self.nodes.len() - 1
    }

    pub fn add_edge(&mut self, from: usize, to: usize) {
        assert!(from < self.nodes.len());
        assert!(to < self.nodes.len());
        self.edges.entry(from).or_default().insert(to);
    }

    pub fn iter_edges_from(&self, from: &str) -> impl Iterator<Item = &str> {
        let Some(idx) = self.nodes.iter().position(|n| n == from) else {
            return None.into_iter().flatten();
        };

        Some(
            self.edges
                .get(&idx)
                .into_iter()
                .flatten()
                .map(|child| &*self.nodes[*child]),
        )
        .into_iter()
        .flatten()
    }

    #[allow(dead_code)]
    pub fn iter_edges(&self) -> impl Iterator<Item = (&str, &str)> {
        self.nodes.iter().enumerate().flat_map(|(node_idx, node)| {
            self.edges
                .get(&node_idx)
                .into_iter()
                .flatten()
                .copied()
                .map(|child_idx| {
                    let child = &*self.nodes[child_idx];
                    (&**node, child)
                })
        })
    }

    pub fn to_dot(&self) -> String {
        let mut out = String::new();

        writeln!(&mut out, "digraph Branches {{").unwrap();
        for (i, node) in self.nodes.iter().enumerate() {
            writeln!(&mut out, "    {i} [label=\"{node}\"];").unwrap();
        }
        for (from, edges) in &self.edges {
            for to in edges {
                writeln!(&mut out, "    {from} -> {to};").unwrap();
            }
        }
        writeln!(&mut out, "}}").unwrap();

        out
    }
}

#[cfg(test)]
mod tests {
    use super::Graph;

    #[test]
    fn get_or_insert_reuses_existing_node() {
        let mut graph = Graph::default();
        let first = graph.get_or_insert("main");
        let second = graph.get_or_insert("main");

        assert_eq!(first, second);
    }

    #[test]
    fn iter_edges_from_returns_empty_for_missing_node() {
        let graph = Graph::default();
        assert_eq!(graph.iter_edges_from("missing").count(), 0);
    }

    #[test]
    fn add_edge_and_iter_edges_from_work() {
        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let feat = graph.get_or_insert("feat");
        graph.add_edge(main, feat);

        let children = graph.iter_edges_from("main").collect::<Vec<_>>();
        assert_eq!(children, vec!["feat"]);
    }

    #[test]
    fn duplicate_edges_are_deduplicated() {
        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let feat = graph.get_or_insert("feat");
        graph.add_edge(main, feat);
        graph.add_edge(main, feat);

        let edges = graph.iter_edges().collect::<Vec<_>>();
        assert_eq!(edges, vec![("main", "feat")]);
    }

    #[test]
    #[should_panic]
    fn add_edge_panics_for_invalid_from_node() {
        let mut graph = Graph::default();
        let feat = graph.get_or_insert("feat");
        graph.add_edge(feat + 1, feat);
    }

    #[test]
    #[should_panic]
    fn add_edge_panics_for_invalid_to_node() {
        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        graph.add_edge(main, main + 1);
    }

    #[test]
    fn to_dot_contains_nodes_and_edges() {
        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let feat = graph.get_or_insert("feat");
        graph.add_edge(main, feat);

        let dot = graph.to_dot();

        assert!(dot.contains("digraph Branches {"));
        assert!(dot.contains("0 [label=\"main\"]"));
        assert!(dot.contains("1 [label=\"feat\"]"));
        assert!(dot.contains("0 -> 1;"));
        assert!(dot.contains("}"));
    }
}
