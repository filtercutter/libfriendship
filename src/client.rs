use routing::{NodeHandle, EffectMeta};

/// Trait for any client that wants to listen in on information that is broadcast
/// from Dispath to the *external* world. This includes notifications of state
/// change (in the routegraph), renderer results, etc.
pub trait Client {
    // TODO: use a multidimensional array type for buffer; remove num_slots param
    fn audio_rendered(&mut self, _buffer: &[f32], _idx: u64, _num_slots: u32) {}
    fn node_queried(&mut self, _handle: &NodeHandle, _meta: &EffectMeta) {}
}
