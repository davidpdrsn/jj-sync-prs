use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

#[derive(Debug, Default)]
pub struct Graph {
    nodes: Vec<String>,
    edges: BTreeMap<usize, BTreeSet<usize>>,
}

#[allow(dead_code)]
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

        writeln!(&mut out, "digraph Workflow {{").unwrap();
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
