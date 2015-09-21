mod effects;

mod effect_node;
mod effect_send;
mod effect_tree;
mod partial;

use std::mem;

fn main() {
    print!("Hello, World!\n");
    print!("Size of effect_node: {}\n", mem::size_of::<effect_node::EffectNode>());
    print!("Size of effect_send: {}\n", mem::size_of::<effect_send::EffectSend>());
    print!("Size of Option<&Effect>: {}\n", mem::size_of::<Option<&effects::effect::Effect>>());
    print!("Size of Option<&u32>: {}\n", mem::size_of::<Option<&u32>>());
    print!("Size of Option<&EffectNode>: {}\n", mem::size_of::<Option<&effect_node::EffectNode>>());
    print!("Size of &u32: {}\n", mem::size_of::<&u32>());
}
