//! Portable heap serialization for durable continuations.
//!
//! This module serializes Nulang heap objects and VM execution state into a
//! portable binary format that can survive Firecracker VM destruction and be
//! deserialized into a fresh VM instance. It is the foundation for NLC's
//! "scale to zero" hibernation model.
//!
//! # Format (big-endian unless noted)
//!
//! ```text
//! Header:
//!   magic:          [u8; 4]  = b"NLCS"
//!   version:        u32      = 1
//!   flags:          u32      (reserved; must be 0)
//!   module_hash:    [u8; 32] (SHA-256 of the CodeModule at capture time;
//!                              zero-filled placeholder until module hashing
//!                              is implemented)
//!   num_objects:    u32
//!   num_strings:    u32      (resolved string table entries)
//!   num_closures:   u32      (closure environment entries)
//!   num_frames:     u32
//!   num_handlers:   u32      (handler stack entries)
//!
//! String table (num_strings entries):
//!   For each:
//!     len:  u16
//!     data: [u8; len]  (UTF-8, NOT null-terminated)
//!
//! Object table (num_objects entries):
//!   For each object (in ID order, 0..num_objects-1):
//!     type_tag:      u8   (TypeTag discriminant)
//!     payload_size:  u32
//!     payload:       [u8; payload_size]
//!       For container types (Array/Record/Tuple/Closure/Map), the payload
//!       is an array of serialized Value entries (each 8 bytes, native
//!       endian). TAG_PTR values have their low 48 bits replaced with the
//!       target object ID (0-based index into this table). TAG_STRING
//!       values have their low 48 bits replaced with the string table index.
//!
//! Closure environments (num_closures entries):
//!   For each:
//!     func_idx:   u32
//!     num_capts:  u16
//!     captures:   [serialized Value; num_capts]  (8 bytes each)
//!
//! Frames (num_frames entries, deepest first = index 0 is current frame):
//!   For each:
//!     pc:           u32
//!     module_idx:   u16
//!     return_dst:   u8
//!     caller_idx:   i32  (-1 = None, else caller frame index in this table)
//!     has_closure:  u8   (0 = None, 1 = Some(index into closure envs))
//!     closure_env:  u32  (only present if has_closure == 1)
//!     num_regs:     u16  (number of non-nil registers to serialize)
//!     reg_indices:  [u16; num_regs]  (register indices, sorted ascending)
//!     reg_values:   [serialized Value; num_regs]  (8 bytes each)
//!
//! Handler stack (num_handlers entries, top-of-stack first):
//!   For each:
//!     handler_table_idx: u32
//!     module_idx:        u16
//!     resume_pc:         u32
//!     resume_dst:        u8
//! ```

use std::collections::HashMap;

use crate::runtime::heap::{ActorHeap, TypeTag};
use crate::value_layout::{self, PAYLOAD_MASK, TAG_CLOSURE, TAG_MASK, TAG_PTR, TAG_STRING};
use crate::vm::{Continuation, Frame, HandlerFrame, Value, VM};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes identifying a Nulang Continuation Serialization payload.
const MAGIC: [u8; 4] = *b"NLCS";

/// Current format version.
const VERSION: u32 = 1;

/// Special sentinel for `caller_idx` meaning "no caller" (None).
const CALLER_NONE: i32 = -1;

// ---------------------------------------------------------------------------
// Serialization context
// ---------------------------------------------------------------------------

/// Context built during the first pass of serialization.
struct SerializeCtx {
    /// Maps payload pointer → object ID.
    obj_ids: HashMap<*const u8, u32>,
    /// String table: deduplicated strings from TAG_STRING values.
    string_table: Vec<String>,
    /// String → index in string_table.
    string_ids: HashMap<String, u32>,
    /// TAG_STRING raw values → string table index.
    string_value_to_idx: HashMap<u64, u32>,
    /// Objects to serialize (in ID order).
    objects: Vec<ObjectInfo>,
    /// Closure environments reachable from frames.
    closures: Vec<ClosureInfo>,
    /// Frames to serialize.
    frames: Vec<FrameInfo>,
    /// Handler stack entries.
    handlers: Vec<HandlerInfo>,
}

struct ObjectInfo {
    type_tag: TypeTag,
    payload_size: u32,
    payload_ptr: *const u8,
}

struct ClosureInfo {
    func_idx: u32,
    captures: Vec<Value>,
}

struct FrameInfo {
    pc: u32,
    module_idx: u16,
    return_dst: u8,
    caller_idx: i32,
    closure_env: Option<u32>, // index into closures table
    regs: [Value; 256],
}

struct HandlerInfo {
    handler_table_idx: u32,
    module_idx: u16,
    resume_pc: u32,
    resume_dst: u8,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Serialize a continuation and handler stack to a portable byte vector.
///
/// The returned bytes can be stored externally (file, NATS KV, Chrono Engine)
/// and later deserialized into a fresh VM via [`deserialize_continuation`].
pub fn serialize_continuation(
    cont: &Continuation,
    handler_stack: &[HandlerFrame],
    vm: &VM,
    module_hash: &[u8; 32],
) -> Result<Vec<u8>, String> {
    let mut ctx = SerializeCtx {
        obj_ids: HashMap::new(),
        string_table: Vec::new(),
        string_ids: HashMap::new(),
        string_value_to_idx: HashMap::new(),
        objects: Vec::new(),
        closures: Vec::new(),
        frames: Vec::new(),
        handlers: Vec::new(),
    };

    // Pass 1: collect all reachable objects from frames and closure envs.
    collect_reachable(&mut ctx, cont, handler_stack, vm)?;

    // Pass 2: serialize to bytes.
    let mut buf = Vec::new();
    write_header(&mut buf, module_hash, &ctx);
    write_string_table(&mut buf, &ctx);
    write_objects(&mut buf, &ctx);
    write_closures(&mut buf, &ctx);
    write_frames(&mut buf, &ctx)?;
    write_handlers(&mut buf, &ctx);

    Ok(buf)
}

/// Deserialize a continuation and handler stack from portable bytes.
///
/// The VM must have the module already loaded. Allocates heap objects via
/// `vm.alloc_on_heap()`.
///
/// Returns the restored continuation and handler stack, ready for
/// installation into the VM.
pub fn deserialize_continuation(
    bytes: &[u8],
    vm: &mut VM,
) -> Result<(Continuation, Vec<HandlerFrame>), String> {
    let mut offset = 0;

    // Read header.
    let (_flags, num_objects, num_strings, num_closures, num_frames, num_handlers) =
        read_header(bytes, &mut offset)?;

    // Read string table.
    let string_table = read_string_table(bytes, &mut offset, num_strings)?;

    // Read and allocate objects.
    let obj_table = read_objects(bytes, &mut offset, num_objects, &string_table, vm)?;

    // Read closure environments.
    let closure_envs = read_closures(
        bytes,
        &mut offset,
        num_closures,
        &string_table,
        &obj_table,
        vm,
    )?;

    // Read frames.
    let frames = read_frames(
        bytes,
        &mut offset,
        num_frames,
        &closure_envs,
        &string_table,
        &obj_table,
        vm,
    )?;

    // Read handler stack.
    let handler_stack = read_handlers(bytes, &mut offset, num_handlers)?;

    // Build continuation. The last frame in the list is the outermost (root).
    // The first frame is the current (innermost).
    let step_count = cont_step_count(&frames);
    let resume_pc = cont_resume_pc(&frames);
    let resume_dst = cont_resume_dst(&frames);

    let cont = Continuation {
        frames,
        current_frame_idx: 0,
        resume_pc,
        resume_dst,
        step_count,
        handler_stack_snapshot: Vec::new(),
    };

    Ok((cont, handler_stack))
}

// ===========================================================================
// Pass 1: Collect reachable objects
// ===========================================================================

fn collect_reachable(
    ctx: &mut SerializeCtx,
    cont: &Continuation,
    handler_stack: &[HandlerFrame],
    vm: &VM,
) -> Result<(), String> {
    // Collect frames.
    for (_fi, frame) in cont.frames.iter().enumerate() {
        let closure_env_idx = resolve_frame_closure(frame, vm, ctx);

        let caller_idx = frame.caller_idx.map(|ci| ci as i32).unwrap_or(CALLER_NONE);

        ctx.frames.push(FrameInfo {
            pc: frame.pc as u32,
            module_idx: frame.module_idx as u16,
            return_dst: frame.return_dst,
            caller_idx,
            closure_env: closure_env_idx,
            regs: frame.regs,
        });

        // Walk register values to discover reachable heap objects.
        for value in &frame.regs {
            walk_value(ctx, *value, vm, frame.module_idx)?;
        }

        // Walk closure env captures (extract before mutable borrow).
        let captures_to_walk: Option<Vec<Value>> = closure_env_idx
            .and_then(|idx| ctx.closures.get(idx as usize))
            .map(|ci| ci.captures.clone());
        if let Some(captures) = captures_to_walk {
            for cap in &captures {
                walk_value(ctx, *cap, vm, frame.module_idx)?;
            }
        }
    }

    // Collect handler stack.
    for hf in handler_stack {
        ctx.handlers.push(HandlerInfo {
            handler_table_idx: hf.handler_table_idx as u32,
            module_idx: hf.module_idx as u16,
            resume_pc: hf.resume_pc as u32,
            resume_dst: hf.resume_dst,
        });
    }

    Ok(())
}

/// Walk a Value, discovering reachable heap objects and strings.
fn walk_value(
    ctx: &mut SerializeCtx,
    value: Value,
    vm: &VM,
    module_idx: usize,
) -> Result<(), String> {
    // Resolve TAG_STRING to string content and add to string table.
    if value.is_string() {
        let content = vm.value_to_string(module_idx, value);
        if !content.is_empty() || value.as_string_id().is_some() {
            if !ctx.string_ids.contains_key(&content) {
                let idx = ctx.string_table.len() as u32;
                ctx.string_table.push(content.clone());
                ctx.string_ids.insert(content, idx);
                ctx.string_value_to_idx.insert(value.as_raw(), idx);
            }
        }
    }

    if let Some(ptr) = value.as_ptr() {
        if ptr.is_null() {
            return Ok(());
        }
        // Already seen this object?
        if ctx.obj_ids.contains_key(&(ptr as *const u8)) {
            return Ok(());
        }

        // SAFETY: ptr is a valid heap payload pointer from the actor's heap.
        let header = unsafe { &*ActorHeap::header_of(ptr) };
        let payload_ptr = ptr;
        let payload_size = header.payload_size as u32;

        let obj_id = ctx.objects.len() as u32;
        ctx.obj_ids.insert(payload_ptr as *const u8, obj_id);
        ctx.objects.push(ObjectInfo {
            type_tag: header.type_tag,
            payload_size,
            payload_ptr: payload_ptr as *const u8,
        });

        // Recursively walk container slots.
        match header.type_tag {
            TypeTag::Array | TypeTag::Record | TypeTag::Tuple | TypeTag::Closure | TypeTag::Map => {
                let slot_count = payload_size as usize / std::mem::size_of::<Value>();
                let slots =
                    unsafe { std::slice::from_raw_parts(payload_ptr as *const Value, slot_count) };
                for slot in slots {
                    walk_value(ctx, *slot, vm, module_idx)?;
                }
            }
            TypeTag::String | TypeTag::ActorRef | TypeTag::RemoteActor | TypeTag::Raw => {}
        }
    }

    // TAG_CLOSURE values: collect the closure environment.
    if value.is_closure() {
        resolve_closure_env(value, vm, ctx);
    }

    Ok(())
}

/// Resolve a TAG_CLOSURE Value to a closure environment index in the
/// serialized closure table. Adds the env if not already present.
fn resolve_closure_env(value: Value, vm: &VM, ctx: &mut SerializeCtx) -> Option<u32> {
    let raw = value.as_raw();
    if (raw & TAG_MASK) != TAG_CLOSURE {
        return None;
    }
    let payload = raw & PAYLOAD_MASK;
    if payload & crate::vm::CLOSURE_ENV_FLAG == 0 {
        return None; // immediate closure, no captures
    }
    let env_idx = (payload & crate::vm::CLOSURE_ENV_IDX_MASK) as usize;
    let env = vm.closure_env(env_idx)?;

    // Check if we already serialized this env.
    for (i, ci) in ctx.closures.iter().enumerate() {
        if ci.func_idx == env.func_idx as u32 && ci.captures == env.captures {
            return Some(i as u32);
        }
    }
    let idx = ctx.closures.len() as u32;
    ctx.closures.push(ClosureInfo {
        func_idx: env.func_idx as u32,
        captures: env.captures.clone(),
    });
    Some(idx)
}

/// Resolve a frame's closure env to a serialized closure index.
fn resolve_frame_closure(frame: &Frame, vm: &VM, ctx: &mut SerializeCtx) -> Option<u32> {
    let v = frame.closure_env?;
    resolve_closure_env(v, vm, ctx)
}

// ===========================================================================
// Pass 2: Write serialized bytes
// ===========================================================================

fn write_header(buf: &mut Vec<u8>, module_hash: &[u8; 32], ctx: &SerializeCtx) {
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // flags
    buf.extend_from_slice(module_hash);
    buf.extend_from_slice(&(ctx.objects.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(ctx.string_table.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(ctx.closures.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(ctx.frames.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(ctx.handlers.len() as u32).to_be_bytes());
}

fn write_string_table(buf: &mut Vec<u8>, ctx: &SerializeCtx) {
    for s in &ctx.string_table {
        let bytes = s.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
}

fn write_objects(buf: &mut Vec<u8>, ctx: &SerializeCtx) {
    for obj in &ctx.objects {
        buf.push(obj.type_tag as u8);
        buf.extend_from_slice(&obj.payload_size.to_be_bytes());

        match obj.type_tag {
            TypeTag::Array
            | TypeTag::Record
            | TypeTag::Tuple
            | TypeTag::Closure
            | TypeTag::Map
            | TypeTag::RemoteActor => {
                // Container: rewrite TAG_PTR and TAG_STRING in Value slots.
                let slot_count = obj.payload_size as usize / std::mem::size_of::<Value>();
                let slots = unsafe {
                    std::slice::from_raw_parts(obj.payload_ptr as *const Value, slot_count)
                };
                for slot in slots {
                    let sv = serialize_one_value(*slot, ctx);
                    buf.extend_from_slice(&sv.to_le_bytes());
                }
            }
            _ => {
                // Non-container: copy payload verbatim.
                let payload_slice = unsafe {
                    std::slice::from_raw_parts(obj.payload_ptr, obj.payload_size as usize)
                };
                buf.extend_from_slice(payload_slice);
            }
        }
    }
}

/// Serialize a single Value into its portable 8-byte form.
///
/// TAG_PTR → low 48 bits = object ID.
/// TAG_STRING → low 48 bits = string table index.
/// All other tags pass through unchanged.
fn serialize_one_value(value: Value, ctx: &SerializeCtx) -> u64 {
    let raw = value.as_raw();
    let tag = raw & TAG_MASK;

    if tag == TAG_PTR {
        if let Some(ptr) = value.as_ptr() {
            if let Some(&obj_id) = ctx.obj_ids.get(&(ptr as *const u8)) {
                return TAG_PTR | (obj_id as u64 & PAYLOAD_MASK);
            }
        }
        // Unreachable pointer — serialize as nil.
        return value_layout::TAG_NIL;
    }

    if tag == TAG_STRING {
        // Resolve TAG_STRING to string table index (populated during walk pass).
        if let Some(&idx) = ctx.string_value_to_idx.get(&raw) {
            return TAG_STRING | (idx as u64 & PAYLOAD_MASK);
        }
    }

    raw
}

fn write_closures(buf: &mut Vec<u8>, ctx: &SerializeCtx) {
    for cl in &ctx.closures {
        buf.extend_from_slice(&cl.func_idx.to_be_bytes());
        buf.extend_from_slice(&(cl.captures.len() as u16).to_be_bytes());
        for cap in &cl.captures {
            let sv = serialize_one_value(*cap, ctx);
            buf.extend_from_slice(&sv.to_le_bytes());
        }
    }
}

fn write_frames(buf: &mut Vec<u8>, ctx: &SerializeCtx) -> Result<(), String> {
    for frame in &ctx.frames {
        buf.extend_from_slice(&frame.pc.to_be_bytes());
        buf.extend_from_slice(&frame.module_idx.to_be_bytes());
        buf.push(frame.return_dst);
        buf.extend_from_slice(&frame.caller_idx.to_be_bytes());

        if let Some(ce) = frame.closure_env {
            buf.push(1u8);
            buf.extend_from_slice(&ce.to_be_bytes());
        } else {
            buf.push(0u8);
        }

        // Collect non-nil registers. nil is the default in Frame::new().
        let mut reg_indices: Vec<u16> = Vec::new();
        let mut reg_values: Vec<u64> = Vec::new();
        for (i, val) in frame.regs.iter().enumerate() {
            if !val.is_nil() {
                reg_indices.push(i as u16);
                reg_values.push(serialize_one_value(*val, ctx));
            }
        }

        buf.extend_from_slice(&(reg_indices.len() as u16).to_be_bytes());
        for &idx in &reg_indices {
            buf.extend_from_slice(&idx.to_be_bytes());
        }
        for &sv in &reg_values {
            buf.extend_from_slice(&sv.to_le_bytes());
        }
    }
    Ok(())
}

fn write_handlers(buf: &mut Vec<u8>, ctx: &SerializeCtx) {
    for h in &ctx.handlers {
        buf.extend_from_slice(&h.handler_table_idx.to_be_bytes());
        buf.extend_from_slice(&h.module_idx.to_be_bytes());
        buf.extend_from_slice(&h.resume_pc.to_be_bytes());
        buf.push(h.resume_dst);
    }
}

// ===========================================================================
// Deserialization
// ===========================================================================

/// Read and validate the header.
fn read_header(bytes: &[u8], offset: &mut usize) -> Result<(u32, u32, u32, u32, u32, u32), String> {
    let min_size = 4 + 4 + 4 + 32 + 4 * 6;
    if bytes.len() < min_size {
        return Err("truncated header".into());
    }

    let magic = &bytes[*offset..*offset + 4];
    if magic != MAGIC {
        return Err(format!("bad magic: expected {:?}, got {:?}", MAGIC, magic));
    }
    *offset += 4;

    let version = read_u32(bytes, offset)?;
    if version != VERSION {
        return Err(format!("unsupported version: {}", version));
    }

    let flags = read_u32(bytes, offset)?;

    // Skip module_hash (32 bytes).
    *offset += 32;

    let num_objects = read_u32(bytes, offset)?;
    let num_strings = read_u32(bytes, offset)?;
    let num_closures = read_u32(bytes, offset)?;
    let num_frames = read_u32(bytes, offset)?;
    let num_handlers = read_u32(bytes, offset)?;

    Ok((
        flags,
        num_objects,
        num_strings,
        num_closures,
        num_frames,
        num_handlers,
    ))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    if *offset + 4 > bytes.len() {
        return Err("truncated u32 field".into());
    }
    let v = u32::from_be_bytes(
        bytes[*offset..*offset + 4]
            .try_into()
            .map_err(|_| "u32 conversion failed".to_string())?,
    );
    *offset += 4;
    Ok(v)
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, String> {
    if *offset + 2 > bytes.len() {
        return Err("truncated u16 field".into());
    }
    let v = u16::from_be_bytes(
        bytes[*offset..*offset + 2]
            .try_into()
            .map_err(|_| "u16 conversion failed".to_string())?,
    );
    *offset += 2;
    Ok(v)
}

fn read_u64_le(bytes: &[u8], offset: &mut usize) -> Result<u64, String> {
    if *offset + 8 > bytes.len() {
        return Err("truncated u64 field".into());
    }
    let v = u64::from_le_bytes(
        bytes[*offset..*offset + 8]
            .try_into()
            .map_err(|_| "u64 conversion failed".to_string())?,
    );
    *offset += 8;
    Ok(v)
}

fn read_string_table(bytes: &[u8], offset: &mut usize, count: u32) -> Result<Vec<String>, String> {
    let mut table = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if *offset + 2 > bytes.len() {
            return Err("truncated string table entry".into());
        }
        let len = read_u16(bytes, offset)? as usize;

        if *offset + len > bytes.len() {
            return Err("truncated string data".into());
        }
        let s = String::from_utf8(bytes[*offset..*offset + len].to_vec())
            .map_err(|e| format!("invalid UTF-8 in string table: {}", e))?;
        *offset += len;
        table.push(s);
    }
    Ok(table)
}

/// Read object table, allocate objects, copy payloads, remap references.
///
/// Returns a map from object ID → new payload pointer.
fn read_objects(
    bytes: &[u8],
    offset: &mut usize,
    count: u32,
    string_table: &[String],
    vm: &mut VM,
) -> Result<HashMap<u32, *mut u8>, String> {
    // Phase 1: allocate all objects (so forward references work).
    let mut obj_table: HashMap<u32, *mut u8> = HashMap::new();
    let mut obj_meta: Vec<(u32, TypeTag, u32, usize)> = Vec::new(); // (id, tag, payload_size, payload_offset)

    for id in 0..count {
        if *offset + 5 > bytes.len() {
            return Err(format!("truncated object {}", id));
        }
        let type_tag_byte = bytes[*offset];
        *offset += 1;
        let payload_size = read_u32(bytes, offset)?;
        let payload_offset = *offset;

        let tag = TypeTag::from_u8(type_tag_byte)
            .ok_or_else(|| format!("unknown type tag {} at object {}", type_tag_byte, id))?;

        let heap_tag = tag; // TypeTag in heap module is the same as HeapTypeTag
        let ptr = vm
            .alloc_on_heap(payload_size as usize, heap_tag)
            .ok_or_else(|| {
                format!(
                    "allocation failed for object {} ({} bytes)",
                    id, payload_size
                )
            })?;

        obj_table.insert(id, ptr);
        obj_meta.push((id, tag, payload_size, payload_offset));

        // Advance past payload.
        *offset += payload_size as usize;
    }

    // Phase 2: copy payloads and remap references.
    for (_id, tag, payload_size, payload_offset) in &obj_meta {
        let ptr = obj_table[&_id];

        match tag {
            TypeTag::Array
            | TypeTag::Record
            | TypeTag::Tuple
            | TypeTag::Closure
            | TypeTag::Map
            | TypeTag::RemoteActor => {
                // Container: copy Values, remapping TAG_PTR and TAG_STRING.
                let slot_count = *payload_size as usize / std::mem::size_of::<Value>();
                let dst_slots =
                    unsafe { std::slice::from_raw_parts_mut(ptr as *mut Value, slot_count) };
                for si in 0..slot_count {
                    let src_start = *payload_offset + si * 8;
                    let raw = u64::from_le_bytes(
                        bytes[src_start..src_start + 8]
                            .try_into()
                            .map_err(|_| "u64 conversion failed".to_string())?,
                    );
                    dst_slots[si] = deserialize_one_value(raw, string_table, &obj_table, vm);
                }
            }
            _ => {
                // Non-container: copy payload verbatim.
                let src = &bytes[*payload_offset..*payload_offset + *payload_size as usize];
                unsafe {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), ptr, *payload_size as usize);
                }
            }
        }
    }

    Ok(obj_table)
}

/// Deserialize a single portable Value back into a real Value.
fn deserialize_one_value(
    raw: u64,
    string_table: &[String],
    obj_table: &HashMap<u32, *mut u8>,
    vm: &mut VM,
) -> Value {
    let tag = raw & TAG_MASK;

    if tag == TAG_PTR {
        let obj_id = (raw & PAYLOAD_MASK) as u32;
        if let Some(&ptr) = obj_table.get(&obj_id) {
            return unsafe { Value::ptr(ptr) };
        }
        // Dangling reference — return nil.
        return Value::nil();
    }

    if tag == TAG_STRING {
        let string_idx = (raw & PAYLOAD_MASK) as u32;
        if (string_idx as usize) < string_table.len() {
            let content = &string_table[string_idx as usize];
            // Intern into the VM's module string pool (module 0 as default).
            return vm.add_runtime_string(0, content.clone());
        }
        // Unknown string — return nil.
        return Value::nil();
    }

    // All other tags pass through unchanged.
    unsafe { Value::from_raw(raw) }
}

fn read_closures(
    bytes: &[u8],
    offset: &mut usize,
    count: u32,
    string_table: &[String],
    obj_table: &HashMap<u32, *mut u8>,
    vm: &mut VM,
) -> Result<Vec<(u32, Vec<Value>)>, String> {
    let mut closures = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if *offset + 6 > bytes.len() {
            return Err("truncated closure entry".into());
        }
        let func_idx = read_u32(bytes, offset)?;
        let num_capts = read_u16(bytes, offset)? as usize;

        let mut captures = Vec::with_capacity(num_capts);
        for _ in 0..num_capts {
            if *offset + 8 > bytes.len() {
                return Err("truncated closure capture".into());
            }
            let raw = read_u64_le(bytes, offset)?;

            captures.push(deserialize_one_value(raw, string_table, obj_table, vm));
        }
        closures.push((func_idx, captures));
    }
    Ok(closures)
}

fn read_frames(
    bytes: &[u8],
    offset: &mut usize,
    count: u32,
    closure_envs: &[(u32, Vec<Value>)],
    string_table: &[String],
    obj_table: &HashMap<u32, *mut u8>,
    vm: &mut VM,
) -> Result<Vec<Frame>, String> {
    let mut frames = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if *offset + 15 > bytes.len() {
            return Err("truncated frame header".into());
        }
        let pc = read_u32(bytes, offset)? as usize;
        let module_idx = read_u16(bytes, offset)? as usize;

        let return_dst = bytes[*offset];
        *offset += 1;
        let caller_idx_raw = i32::from_be_bytes(
            bytes[*offset..*offset + 4]
                .try_into()
                .map_err(|_| "i32 conversion failed".to_string())?,
        );
        *offset += 4;
        let has_closure = bytes[*offset];
        *offset += 1;

        let closure_env = if has_closure == 1 {
            if *offset + 4 > bytes.len() {
                return Err("truncated closure env index".into());
            }
            let ce_idx = read_u32(bytes, offset)? as usize;
            if ce_idx < closure_envs.len() {
                let (func_idx, captures) = &closure_envs[ce_idx];
                let env_idx = vm.closure_env_count();
                vm.push_closure_env(crate::vm::ClosureEnv {
                    func_idx: *func_idx as usize,
                    captures: captures.clone(),
                });
                Some(unsafe {
                    Value::from_raw(
                        TAG_CLOSURE
                            | crate::vm::CLOSURE_ENV_FLAG
                            | (env_idx as u64 & crate::vm::CLOSURE_ENV_IDX_MASK),
                    )
                })
            } else {
                None
            }
        } else {
            None
        };

        let caller_idx = if caller_idx_raw == CALLER_NONE {
            None
        } else {
            Some(caller_idx_raw as usize)
        };

        let num_regs = read_u16(bytes, offset)? as usize;

        // Read register indices.
        let mut reg_indices = Vec::with_capacity(num_regs);
        for _ in 0..num_regs {
            if *offset + 2 > bytes.len() {
                return Err("truncated reg index".into());
            }
            reg_indices.push(read_u16(bytes, offset)? as usize);
        }

        // Read register values.
        let mut reg_values = Vec::with_capacity(num_regs);
        for _ in 0..num_regs {
            if *offset + 8 > bytes.len() {
                return Err("truncated reg value".into());
            }
            let raw = read_u64_le(bytes, offset)?;

            reg_values.push(deserialize_one_value(raw, string_table, obj_table, vm));
        }

        // Build frame with nil defaults, then fill in non-nil regs.
        let mut frame = Frame::new(caller_idx, module_idx);
        frame.pc = pc;
        frame.return_dst = return_dst;
        frame.closure_env = closure_env;
        for (idx, val) in reg_indices.iter().zip(reg_values.iter()) {
            frame.regs[*idx] = *val;
        }

        frames.push(frame);
    }
    Ok(frames)
}

fn read_handlers(
    bytes: &[u8],
    offset: &mut usize,
    count: u32,
) -> Result<Vec<HandlerFrame>, String> {
    let mut handlers = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if *offset + 11 > bytes.len() {
            return Err("truncated handler entry".into());
        }
        let handler_table_idx = read_u32(bytes, offset)? as usize;
        let module_idx = read_u16(bytes, offset)? as usize;

        let resume_pc = read_u32(bytes, offset)? as usize;
        let resume_dst = bytes[*offset];
        *offset += 1;

        handlers.push(HandlerFrame::new(
            handler_table_idx,
            module_idx,
            resume_pc,
            resume_dst,
        ));
    }
    Ok(handlers)
}

// ---------------------------------------------------------------------------
// Helpers for building Continuation from deserialized frames
// ---------------------------------------------------------------------------

fn cont_step_count(_frames: &[Frame]) -> usize {
    // The step count is not critical for correctness; use 0.
    0
}

fn cont_resume_pc(frames: &[Frame]) -> usize {
    frames.first().map(|f| f.pc).unwrap_or(0)
}

fn cont_resume_dst(frames: &[Frame]) -> u8 {
    frames.first().map(|f| f.return_dst).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// TypeTag helpers
// ---------------------------------------------------------------------------

impl TypeTag {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TypeTag::ActorRef),
            1 => Some(TypeTag::Array),
            2 => Some(TypeTag::String),
            3 => Some(TypeTag::Record),
            4 => Some(TypeTag::Closure),
            5 => Some(TypeTag::Map),
            6 => Some(TypeTag::Tuple),
            7 => Some(TypeTag::Raw),
            8 => Some(TypeTag::RemoteActor),
            _ => None,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_constants() {
        assert_eq!(MAGIC, *b"NLCS");
        assert_eq!(VERSION, 1);
        assert_eq!(CALLER_NONE, -1);
    }

    #[test]
    fn test_typettag_roundtrip() {
        let tags = [
            TypeTag::ActorRef,
            TypeTag::Array,
            TypeTag::String,
            TypeTag::Record,
            TypeTag::Closure,
            TypeTag::Map,
            TypeTag::Tuple,
            TypeTag::Raw,
            TypeTag::RemoteActor,
        ];
        for tag in tags {
            let v = tag as u8;
            let back = TypeTag::from_u8(v).unwrap();
            assert_eq!(back, tag);
        }
        assert!(TypeTag::from_u8(255).is_none());
    }
}
