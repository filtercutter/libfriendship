use routing::{NodeHandle, Edge, EdgeWeight, EffectId, EffectDesc, EffectMeta, EffectInput, EffectOutput};
use routing::AdjList;

/// Get the EffectDesc for the Passthrough effect.
/// The passthrough effect takes all data input to slot 0 and sends it to
/// output slot 0.
pub fn get_desc() -> EffectDesc {
    let edge = Edge::new_from_null(NodeHandle::toplevel(), EdgeWeight::new(0, 0));
    let nodes = [];
    let edges = [edge];

    let list = AdjList {
        nodes: nodes.iter().cloned().collect(),
        edges: edges.iter().cloned().collect(),
    };
    EffectDesc::new(EffectMeta::new("Passthrough".into(), None,
        collect_arr!{[ (0, EffectInput::new("source".into(), 0)) ]},
        collect_arr!{[ (0, EffectOutput::new("result".into(), 0)) ]},
    ), list)
}

pub fn get_id() -> EffectId {
    get_desc().id()
}
