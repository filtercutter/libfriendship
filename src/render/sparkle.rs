//! Sparkle is a JIT-based renderer that uses llvm to compile effects as they
//! are loaded.
//! Other than the JIT aspect, it is mostly a literal reimplementation of
//! the reference renderer.

use std::collections::HashMap;
use std::ffi::CString;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr;

use jagged_array::Jagged2;
use llvm;
use llvm::{Builder, Context, ContextType, ExecutionEngine, Function, Module};
use llvm_sys;
use llvm_sys::core::{
    LLVMContextCreate,
    LLVMContextDispose,
    LLVMDisposeModule,
    LLVMModuleCreateWithNameInContext,
    LLVMStructCreateNamed,
    LLVMStructSetBody,
};
use llvm_sys::{
    LLVMIntPredicate,
    LLVMRealPredicate,
};
use llvm_sys::prelude::*;
use llvm_sys::target::{LLVM_InitializeNativeTarget, LLVM_InitializeNativeAsmPrinter,
                   LLVM_InitializeNativeAsmParser};
use ndarray::Array2;
use streaming_iterator::StreamingIterator;

use render::Renderer;
use resman::AudioBuffer;
use routing::{Edge, Effect, GraphWatcher, NodeData, NodeHandle, RouteGraph};
use routing::effect::{PrimitiveEffect, EffectData};



#[derive(Debug)]
pub struct SparkleRenderer {
    // Top-level node map
    nodes: NodeMap,
    /// inputs[slot][sample] represents the value of the external input at
    /// the given slot and time. On OOB, it is assumed to be 0.
    inputs: Vec<Vec<f32>>,
    /// Next expected sample to be queried.
    /// This is tracked because if we do a seek, the inputs need to be zero'd.
    head: u64,
    // LLVM data below
    /// Llvm execution engines. Zipped against the modules.
    llvm_engines: Vec<ExecutionEngine>,
    /// Finalized llvm modules.
    llvm_modules: Vec<Module>,
    /// Module that has yet to be compiled.
    open_module: Option<Module>,
    /// LLVM struct { fn(time, slot, callback_type*)->f32, callback_type* }
    /// Used to pass callbak functions into get_output() to allow effects to access their inputs.
    callback_type: LLVMTypeRef,
    /// LLVM type for fn(time, slot, input_getter: callback_type*) -> f32
    sample_getter_type: LLVMTypeRef,
    /// Object that lets us build IR into basic blocks.
    llvm_builder: Builder,
    // NOTE: LLVM Context must be last member, otherwise jemalloc will try dropping
    // llvm-owned data
    /// Object that provides a context for LLVM calls.
    llvm_ctx: Context,
}

#[derive(Debug, Default)]
struct NodeMap {
    nodes: HashMap<NodeHandle, Node>,
    output_edges: Vec<Option<Edge>>,
}

#[derive(Debug)]
struct Node {
    data: MyNodeData,
    /// Inbound edges, indexed by slot idx.
    inbound: Vec<Option<Edge>>,
}

#[derive(Debug)]
enum MyNodeData {
    /// The node corresponds to a LLVM function with the provided name.
    LlvmFunc(String),
    /// External audio.
    Buffer(AudioBuffer),
}

#[derive(Copy, Clone)]
struct CallbackType {
    input_getter: *const fn(u64, u32, *const CallbackType) -> f32,
    userdata: *const CallbackType,
}

impl Renderer for SparkleRenderer {
    fn fill_buffer(&mut self, buff: &mut Array2<f32>, idx: u64, inputs: Jagged2<f32>) {
        let (n_slots, n_times) = buff.dim().into();
        // Store inputs for future use
        {
            // If this is a seek operation, forget historical input values.
            if idx != self.head {
                for slot in &mut self.inputs {
                    *slot = Vec::new();
                    // NB: Improper indexing on 32-bit OS, but will OOM first.
                    slot.resize(idx as usize, 0f32);
                }
            }
            // Make sure we have storage space for all inputs.
            while self.inputs.len() < buff.len() {
                // NB: Improper indexing on 32-bit OS, but will OOM first.
                let mut v = Vec::with_capacity(idx as usize);
                v.resize(idx as usize, 0f32);
                self.inputs.push(v);
            }
            let mut stream = inputs.stream();
            let mut self_it = self.inputs.iter_mut();
            while let (Some(row), Some(vec_dest)) = (stream.next(), self_it.next()) {
                assert_eq!(vec_dest.len(), idx as usize);
                vec_dest.extend(row);
                assert!(vec_dest.len() <= idx as usize + n_times); // cannot send inputs ahead of outputs.
                let pad_val = vec_dest.last().cloned().unwrap_or(0f32);
                vec_dest.resize(idx as usize +n_times, pad_val);
            }
        }

        // Calculate outputs
        self.prep_execution();
        for slot in 0..n_slots as u32 {
            for time in idx..idx+n_times as u64 {
                buff[[slot as usize, (time - idx) as usize]] = self.get_sample(time, slot);
            }
        }
        // Keep track of the playhead
        self.head = idx + n_times as u64;
    }
}

impl GraphWatcher for SparkleRenderer {
    fn on_add_node(&mut self, handle: &NodeHandle, data: &NodeData) {
        let my_node_data = self.make_node(data);
        self.nodes.insert(*handle, Node::new(my_node_data));
    }
    fn on_del_node(&mut self, handle: &NodeHandle) {
        self.nodes.remove(handle);
    }
    fn on_add_edge(&mut self, edge: &Edge) {
        self.nodes.add_edge(edge);
    }
    fn on_del_edge(&mut self, edge: &Edge) {
        let inbound = if edge.to_full().is_toplevel() {
            &mut self.nodes.output_edges
        } else {
            &mut self.nodes.get_mut(&edge.to_full()).expect("Attempt to delete edge, but it was never created!").inbound
        };
        if let Some(stored_edge) = inbound.get_mut(edge.to_slot() as usize) {
            *stored_edge = None
        }
    }
}

impl SparkleRenderer {
    /// Creates a LLVM function with signature:
    /// fn get_sample(time: u64, slot: u32, input_getter: &callback_type) -> f32) -> f32
    /// callback_type should be { input_getter, userdata },
    /// Returns the name of the function that can be used to get the effect's output.
    fn jit_effect(&mut self, effect: &Effect) -> String {
        let fname = format!("{}_get_output", effect.id().name());
        println!("jit: {}", fname);
        if self.get_func(&fname).is_none() {
            // Effect hasn't been compiled yet; do so.
            // TODO: this only checks the execution engines; not uncompiled modules!
            let mut func = self.open_get_sample_fn(&fname);
            // Open a basic block to begin appending instructions.
            let bb = self.llvm_ctx.append_basic_block(&mut func, &fname);
            let ref mut builder = self.llvm_builder;
            builder.position_at_end(bb);
            let time = func.get_param(0).unwrap();
            let slot = func.get_param(1).unwrap();
            let in_getter = func.get_param(2).unwrap();
            // Common constants used across primitives
            let u32_0 = self.llvm_ctx.cons(0u32);
            let u32_1 = self.llvm_ctx.cons(1u32);
            let u64_0 = self.llvm_ctx.cons(0u64);
            let f32_0 = self.llvm_ctx.cons(0f32);
            let f32_2pow64 = self.llvm_ctx.cons(18446744073709551616f32);

            let load_getters = |builder: &mut Builder| -> (LLVMValueRef, LLVMValueRef) {
                let in_getter_struct = builder.build_load(in_getter, "in_getter_struct");
                let in_getter_fn = builder.build_extract_value(in_getter_struct, 0, "in_getter_fn");
                let in_getter_arg = builder.build_extract_value(in_getter_struct, 1, "in_getter_arg");
                (in_getter_fn, in_getter_arg)
            };

            match *effect.data() {
                EffectData::Primitive(prim) => match prim {
                    PrimitiveEffect::F32Constant => {
                        let f32_type = f32::get_type_in_context(&self.llvm_ctx);
                        let slot_as_f32 = builder.build_bit_cast(slot, f32_type, "slot_as_f32");
                        builder.build_ret(slot_as_f32);
                    },
                    PrimitiveEffect::Delay => {
                        let bb_ret0 = self.llvm_ctx.append_basic_block(&mut func, &(fname.clone() + "_ret0"));
                        let bb_not_too_large = self.llvm_ctx.append_basic_block(&mut func, &(fname.clone() + "_not_too_large"));
                        let bb_not_gt_time = self.llvm_ctx.append_basic_block(&mut func, &(fname.clone() + "_not_gt_time"));
                        // TODO: guard against slot != 0
                        let (in_getter_fn, in_getter_arg) = load_getters(builder);
                        let delay_frames = builder.build_call(Function::from_value_ref(in_getter_fn),
                            vec![time, u32_1, in_getter_arg], "delay_frames");
                        let is_too_large = builder.build_fcmp(LLVMRealPredicate::LLVMRealUGT,
                            delay_frames,
                            f32_2pow64,
                            "is_too_large");
                        builder.build_cond_br(is_too_large, bb_ret0, bb_not_too_large);

                        // Impl the case where `delay_frames` >= 2^64
                        builder.position_at_end(bb_ret0);
                        builder.build_ret(f32_0);

                        // Impl the case where `delay_frames < 2^64`
                        builder.position_at_end(bb_not_too_large);
                        // Clamp delay_frames to [0, x) & cast to u64
                        let is_lt_0 = builder.build_fcmp(LLVMRealPredicate::LLVMRealULT,
                            delay_frames,
                            f32_0,
                            "is_lt_0");
                        let unchecked_delay_iframes = builder.build_fp_to_ui(delay_frames,
                            u64::get_type_in_context(&self.llvm_ctx),
                            "unchecked_delay_iframes");
                        let delay_iframes = builder.build_select(is_lt_0, u64_0, unchecked_delay_iframes, "delay_iframes");
                        let is_gt_time = builder.build_icmp(LLVMIntPredicate::LLVMIntUGT,
                            delay_iframes,
                            time,
                            "is_gt_time");
                        builder.build_cond_br(is_gt_time, bb_ret0, bb_not_gt_time);

                        // Impl the case where the delay amount is valid
                        builder.position_at_end(bb_not_gt_time);
                        let delayed_time = builder.build_sub(time, delay_iframes, "delayed_time");

                        let delayed_input = builder.build_call(Function::from_value_ref(in_getter_fn),
                            vec![delayed_time, u32_0, in_getter_arg], "delayed_input");
                        builder.build_ret(delayed_input);
                    },
                    PrimitiveEffect::Sum2 => {
                        // TODO: guard against slot != 0
                        let (in_getter_fn, in_getter_arg) = load_getters(builder);
                        let input_slot0 = builder.build_call(Function::from_value_ref(in_getter_fn),
                            vec![time, u32_0, in_getter_arg], "input_slot0");
                        let input_slot1 = builder.build_call(Function::from_value_ref(in_getter_fn),
                            vec![time, u32_1, in_getter_arg], "input_slot1");
                        let sum = builder.build_fadd(input_slot0, input_slot1, "sum");
                        builder.build_ret(sum);
                    },
                    _ => {
                        let ret = self.llvm_ctx.cons(0f32);
                        builder.build_ret(ret);
                    },
                },
                EffectData::RouteGraph(ref graph) => {
                    // TODO: for now, just use a dummy return value
                    let ret = self.llvm_ctx.cons(20f32);
                    builder.build_ret(ret);
                },
                _ => panic!("Cannot JIT effect: {:?}", effect)
            }
        }

        fname
    }
    fn open_get_sample_fn(&mut self, fname: &str) -> Function {
        let ty = self.sample_getter_type;
        self.get_open_module().add_function(ty, fname)
    }
    /// Get or create an editable module
    fn get_open_module(&mut self) -> &mut Module {
        // TODO: use entry API
        if let Some(ref mut module) = self.open_module {
            module
        } else {
            self.open_module = Some(
                self.llvm_ctx.module_create_with_name(
                    &format!("mod{}", self.llvm_modules.len()))
            );
            self.open_module.as_mut().unwrap()
        }
    }
    /// Return a pointer to the compiled function with the provided name.
    /// Search across all execution engines.
    fn get_func(&self, name: &str) -> Option<extern "C" fn()> {
        self.llvm_engines.iter().flat_map(|ee| {
            ee.get_function_address(name).into_iter()
        }).next()
    }
    /// Compile all outstanding functions.
    fn prep_execution(&mut self) {
        // IF there's an open module, compile it.
        if let Some(module) = self.open_module.take() {
            let ee = {
                module.dump();
                llvm::ExecutionEngine::create_for_module(&module).unwrap()
            };
            self.llvm_engines.push(ee);
        }
    }
    /// Allocate renderer data based on data from a RouteGraph node.
    fn make_node(&mut self, effect: &NodeData) -> MyNodeData {
        match *effect.data() {
            EffectData::Buffer(ref buff) => MyNodeData::Buffer(buff.clone()),
            EffectData::Primitive(_e) =>
                MyNodeData::LlvmFunc(self.jit_effect(effect)),
            EffectData::RouteGraph(ref _graph) =>
                MyNodeData::LlvmFunc(self.jit_effect(effect)),
        }
    }
    /// Get the output at a particular time and to a particular output slot.
    fn get_sample(&mut self, time: u64, slot: u32) -> f32 {
        let out_edge = self.nodes.output_edges.get(slot as usize);
        self.get_maybe_edge_value(time, out_edge)
    }
    /// Wrapper around `get_edge_value` that will return 0f32 if maybe_edge is not
    /// `Some(&Some(edge))`.
    fn get_maybe_edge_value(&self, time: u64,
        maybe_edge: Option<&Option<Edge>>) -> f32
    {
        if let Some(&Some(ref edge)) = maybe_edge {
            self.get_edge_value(time, &edge)
        } else {
            // Edge doesn't exist; value is zero.
            0f32
        }
    }
    /// Get the value on an edge at a specific time.
    /// This will recurse down, all the way to the input to this node itself.
    fn get_edge_value(&self, time: u64, edge: &Edge) -> f32 {
        let from = edge.from_full();
        let from_slot = edge.from_slot();
        if *from.node_handle() == None {
            println!("Read from input: {}, {}", time, from_slot);
            // reading from an input
            *self.inputs.get(from_slot as usize)
                .and_then(|v| v.get(time as usize))
                .unwrap_or(&0f32)
        } else {
            // Reading from another node within the DAG
            let node = &self.nodes[&from];
            match node.data {
                MyNodeData::LlvmFunc(ref fname) => {
                    let out_getter = self.get_func(fname);
                    out_getter.map(|getter| unsafe {
                        let in_edge_getter = |time2: u64, slot2: u32| {
                            // get the input to this node.
                            let in_edge = node.inbound.get(slot2 as usize);
                            self.get_maybe_edge_value(time2, in_edge)
                        };
                        let f: extern "C" fn(u64, u32, *const CallbackType) -> f32 = mem::transmute(getter);
                        let callback = CallbackType {
                            input_getter: call_closure_from_c as *const fn(u64, u32, *const CallbackType) -> f32,
                            userdata: &mem::transmute(&in_edge_getter as &Fn(u64, u32) -> f32),
                        };
                        f(time, from_slot, &callback)
                    }).unwrap()
                }
                MyNodeData::Buffer(ref buf) => buf.get(time, from_slot),
            }
        }
    }
}

extern "C" fn call_closure_from_c(time: u64, slot: u32, closure_info: *const CallbackType) -> f32 {
    unsafe {
        let closure: &Fn(u64, u32) -> f32 = mem::transmute(*closure_info);
        closure(time, slot)
    }
}


impl Default for SparkleRenderer {
    fn default() -> SparkleRenderer {
        // Initialize core LLVM features
        llvm::link_in_mcjit();
        llvm::initialize_native_target();
        llvm::initialize_native_asm_printer();

        let llvm_ctx = Context::new();
        let llvm_builder = llvm_ctx.create_builder();
        let llvm_modules = Default::default();
        let llvm_engines = Default::default();
        let open_module = None;

        // Create the callback_type struct.
        // It is recursive, and has a dependency on some fn-ptrs,
        // so its creation is staggered.
        let callback_type = {
            let c_name = CString::new("SampleGetter").unwrap();
            unsafe {
                LLVMStructCreateNamed(llvm_ctx.ptr, c_name.as_ptr())
            }
        };

        let sample_getter_type = llvm::function_type(
            f32::get_type_in_context(&llvm_ctx),
            vec![
                u64::get_type_in_context(&llvm_ctx),
                u32::get_type_in_context(&llvm_ctx),
                llvm::pointer_type(callback_type, 0)
            ],
        /* is_var_arg */false);

        unsafe {
            let mut element_types = vec![llvm::pointer_type(sample_getter_type, 0), llvm::pointer_type(callback_type, 0)];
            let is_packed = false;
            LLVMStructSetBody(callback_type, element_types.as_mut_ptr(),
                element_types.len() as u32, is_packed as i32)
        }

        let (head, inputs, nodes) = Default::default();

        SparkleRenderer {
            head, inputs, nodes,
            llvm_ctx, llvm_builder, llvm_modules, llvm_engines, open_module, callback_type, sample_getter_type
        }
    }
}

impl NodeMap {
    /// Add an edge that connects two nodes within this graph.
    fn add_edge(&mut self, edge: &Edge) {
        let inbound = if edge.to_full().is_toplevel() {
            &mut self.output_edges
        } else {
            &mut self.nodes.get_mut(&edge.to_full()).unwrap().inbound
        };
        let slot: u32 = edge.to_slot().into();
        let slot = slot as usize;
        // allocate space to store the edge.
        if inbound.len() <= slot { inbound.resize(slot+1, None) }

        inbound[slot] = Some(edge.clone());
    }
}

impl Node {
    fn new(data: MyNodeData) -> Self {
        Node {
            data: data,
            inbound: Vec::new(),
        }
    }
}

impl Deref for NodeMap {
    type Target = HashMap<NodeHandle, Node>;
    fn deref(&self) -> &Self::Target {
        &self.nodes
    }
}

impl DerefMut for NodeMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.nodes
    }
}
