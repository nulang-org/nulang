#!/usr/bin/env python3
from pathlib import Path

path = Path("crates/nulang-ui-protocol/src/lib.rs")
text = path.read_text()

old = '''        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        visit(&self.root, &nodes, &mut visiting, &mut visited)?;
        if visited.len() != nodes.len() {
'''
new = '''        let visited = visit_iterative(&self.root, &nodes)?;
        if visited.len() != nodes.len() {
'''
if text.count(old) != 1:
    raise SystemExit(f"validation traversal marker count: {text.count(old)}")
text = text.replace(old, new, 1)

old = '''fn visit(
    node_id: &NodeId,
    nodes: &BTreeMap<NodeId, &UiNode>,
    visiting: &mut BTreeSet<NodeId>,
    visited: &mut BTreeSet<NodeId>,
) -> Result<(), ProtocolError> {
    if visited.contains(node_id) {
        return Ok(());
    }
    if !visiting.insert(node_id.clone()) {
        return Err(ProtocolError::Cycle(node_id.clone()));
    }
    for child in &nodes
        .get(node_id)
        .expect("child references are checked before traversal")
        .children
    {
        visit(child, nodes, visiting, visited)?;
    }
    visiting.remove(node_id);
    visited.insert(node_id.clone());
    Ok(())
}
'''
new = '''fn visit_iterative(
    root: &NodeId,
    nodes: &BTreeMap<NodeId, &UiNode>,
) -> Result<BTreeSet<NodeId>, ProtocolError> {
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = vec![(root.clone(), false)];

    while let Some((node_id, exiting)) = stack.pop() {
        if exiting {
            visiting.remove(&node_id);
            visited.insert(node_id);
            continue;
        }
        if visited.contains(&node_id) {
            continue;
        }
        if !visiting.insert(node_id.clone()) {
            return Err(ProtocolError::Cycle(node_id));
        }

        stack.push((node_id.clone(), true));
        let children = &nodes
            .get(&node_id)
            .expect("child references are checked before traversal")
            .children;
        for child in children.iter().rev() {
            if visiting.contains(child) {
                return Err(ProtocolError::Cycle(child.clone()));
            }
            if !visited.contains(child) {
                stack.push((child.clone(), false));
            }
        }
    }

    Ok(visited)
}
'''
if text.count(old) != 1:
    raise SystemExit(f"recursive visit marker count: {text.count(old)}")
text = text.replace(old, new, 1)

marker = '''    #[test]
    fn runtime_message_round_trip_preserves_patch_order() {
'''
test = '''    #[test]
    fn tree_validation_handles_deep_documents_iteratively() {
        const DEPTH: usize = 20_000;
        let mut nodes = Vec::with_capacity(DEPTH);
        for index in 0..DEPTH {
            let id = NodeId::new(format!("node-{index:05}"));
            let mut node = UiNode::new(id, "column");
            if index + 1 < DEPTH {
                node.children
                    .push(NodeId::new(format!("node-{:05}", index + 1)));
            }
            nodes.push(node);
        }
        let document = UiDocument::new(
            DocumentId::from("deep"),
            Revision(1),
            NodeId::from("node-00000"),
            nodes,
        );
        document.validate().unwrap();
    }

'''
if text.count(marker) != 1:
    raise SystemExit(f"test insertion marker count: {text.count(marker)}")
text = text.replace(marker, test + marker, 1)

path.write_text(text)
print("iterative protocol validation fix applied")
