use std::f32;

use partial::Partial;
use phaser::PhaserCoeff;
use real::Real32;
use render::render_spec::RenderSpecFactory;
use render::reference::tree_renderer::TreeRenderer;
use tree::node::YNode;
use tree::send::Send;
use tree::tree::Tree;

use super::approx_equal::assert_similar_audio;

/// Try to render a single 440 Hz sine wave through the reference tree_renderer
pub fn get_basic_sine(n_samples : u32) -> Vec<f32> {
    let render_spec = RenderSpecFactory::new().sample_rate(44100).finalize();
    let mut tree = TreeRenderer::new(render_spec);
    let exit_node = YNode::new_rc();
    tree.watch_nodes(&vec![exit_node.clone()]);
    
    // inject the sine wave directly into the output
    tree.add_send(
        Send::new_ysrcsend(
            Partial::new(PhaserCoeff::new_f32(0f32, -1f32), Real32::new(440f32*2f32*f32::consts::PI)),
            exit_node.clone()
        )
    );


    let mut samples = vec![];
    for _ in (0..n_samples) {
        samples.push(tree.step()[0]);
    }
    samples
}

#[test]
pub fn test_basic_sine() {
    let n_samples = 100;
    let actual = get_basic_sine(n_samples);
    let mut reference = vec![];
    for i in (0..n_samples) {
        reference.push((440f32*2f32*f32::consts::PI * (i as f32) / 44100f32).sin());
    }

    assert_similar_audio(&reference, &actual);
}

