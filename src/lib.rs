//! Wobbly windows for miracle-wm.
//!
//! As a window slides to a new position it bends like a sheet of jelly: its
//! corners trail behind while it moves, swing past when it stops, and wobble back
//! and forth a little before settling.
//!
//! The compositor's built-in move animation still does the moving: the plugin only
//! watches where the built-in animation puts the window each frame
//! ([`AnimationFrameData::builtin`]) and returns `None` so the frame is left to it.
//!
//! The wobble itself is a pair of damped springs: one follows the window's
//! top-left corner and the other its bottom-right corner. Each spring pulls a
//! virtual point toward its corner; how far the point lags behind (the "lag") is
//! sent to a geometry shader, which subdivides the window and displaces each
//! vertex by a blend of the two lags (the top-left lag on the left/top edges, the
//! bottom-right lag on the right/bottom edges), weighted from nothing at the
//! window's center to all of it at the corners. Following both corners means a
//! window that is only resized (say, the left of two side-by-side windows, whose
//! top-left corner stays put) still wobbles along the edge that moves. The
//! springs keep ticking after the move ends (via a custom animation) until they
//! come to rest, then the shader is detached.

use std::cell::RefCell;
use std::collections::HashMap;

use glam::Vec2;
use miracle_plugin::animation::{AnimationFrameData, AnimationFrameResult};
use miracle_plugin::plugin::Plugin;
use miracle_plugin::window::{ShaderUniform, Window};
use miracle_plugin::{
    cancel_custom_animation, queue_custom_animation, register_window_geometry_shader,
};

/// Spring stiffness, in 1/s². Sets how fast the window wobbles (~2.5 Hz).
const STIFFNESS: f32 = 250.0;
/// Spring damping, in 1/s. Sets how quickly the wobble dies down (damping ratio ~0.25).
const DAMPING: f32 = 7.9;
/// The largest lag, in pixels, however fast the window moves.
const MAX_LAG: f32 = 80.0;
/// The largest lag as a fraction of the window's smaller side, so that small
/// windows are not folded over.
const MAX_LAG_FRACTION: f32 = 0.15;
/// The spring is at rest once both its lag (px) and speed (px/s) are below these.
const REST_LAG: f32 = 0.5;
const REST_SPEED: f32 = 5.0;
/// Integration step, in seconds. Small enough to stay stable at [STIFFNESS].
const STEP: f32 = 1.0 / 240.0;
/// Frames further apart than this (e.g. after a stall) are integrated as if they
/// were this long, so that the spring does not jump.
const MAX_FRAME: f32 = 1.0 / 30.0;
/// A wobble is stopped after this long even if it has not come to rest.
const MAX_WOBBLE_SECONDS: f32 = 10.0;

/// Subdivides each incoming triangle into an 8x8 grid so that the window can bend,
/// and displaces every vertex by the lag of the corners: `u_lag_lo` (top-left)
/// and `u_lag_hi` (bottom-right), blended per axis by the vertex's position across
/// the window and weighted by its distance from the window's center. When the
/// window only moves, both lags are equal and the window trails as one piece; when
/// it is resized, only the moving edge trails. Everything only depends on
/// `v_local`, so the window's content and its border bend identically.
///
/// Each of the 8 rows is emitted as one strip of 2 * (8 - row) + 1 vertices, which
/// is 80 vertices in all.
const WOBBLE_GEOMETRY_SHADER: &str = r#"#version 320 es
precision highp float;

layout(triangles) in;
layout(triangle_strip, max_vertices = 80) out;

in vec2 v_texcoord[];
in vec2 v_local[];
in vec4 v_world[];
out vec2 g_texcoord;

uniform mat4 u_world_to_clip;
uniform vec2 u_lag_lo;
uniform vec2 u_lag_hi;

const int N = 8;

// Emits the vertex at barycentric coordinates (1 - a - b, a, b).
void emit(float a, float b) {
    float c = 1.0 - a - b;
    vec4 world = c * v_world[0] + a * v_world[1] + b * v_world[2];
    vec2 local = c * v_local[0] + a * v_local[1] + b * v_local[2];

    // 0 at the center of the window, 1 at its corners.
    vec2 centered = local * 2.0 - 1.0;
    float weight = 0.5 * dot(centered, centered);
    world.xy += mix(u_lag_lo, u_lag_hi, local) * weight;

    gl_Position = u_world_to_clip * world;
    g_texcoord = c * v_texcoord[0] + a * v_texcoord[1] + b * v_texcoord[2];
    EmitVertex();
}

void main() {
    for (int row = 0; row < N; ++row) {
        float b0 = float(row) / float(N);
        float b1 = float(row + 1) / float(N);
        for (int column = 0; column < N - row; ++column) {
            float a = float(column) / float(N);
            emit(a, b0);
            emit(a, b1);
        }
        emit(float(N - row) / float(N), b0);
        EndPrimitive();
    }
}
"#;

/// A damped spring whose virtual point trails one corner of a window.
struct Spring {
    /// The virtual point that trails the corner, and its velocity.
    position: Vec2,
    velocity: Vec2,
    /// Where the corner is, as last reported by the move animation.
    target: Vec2,
    /// [target] as of the previous tick, to tell whether the corner is still moving.
    previous_target: Vec2,
}

impl Spring {
    fn new(target: Vec2) -> Self {
        Self {
            position: target,
            velocity: Vec2::ZERO,
            target,
            previous_target: target,
        }
    }

    fn lag(&self) -> Vec2 {
        self.position - self.target
    }

    /// Advances the spring by `dt` seconds, keeping its lag within `max_lag`.
    fn step(&mut self, dt: f32, max_lag: f32) {
        let mut remaining = dt.min(MAX_FRAME);
        while remaining > 0.0 {
            let h = remaining.min(STEP);
            // Semi-implicit Euler: update the velocity first, then move with it.
            let acceleration = -STIFFNESS * self.lag() - DAMPING * self.velocity;
            self.velocity += acceleration * h;
            self.position += self.velocity * h;
            remaining -= h;
        }

        // Clamp the lag so that very fast moves do not turn the window inside out.
        let lag = self.lag();
        if lag.length() > max_lag {
            self.position = self.target + lag.normalize() * max_lag;
        }
    }

    fn is_at_rest(&self) -> bool {
        self.target == self.previous_target
            && self.lag().length() < REST_LAG
            && self.velocity.length() < REST_SPEED
    }
}

/// The two springs behind one window's wobble.
struct Wobble {
    window: Window,
    /// The custom animation that ticks the springs.
    animation_id: u32,
    /// Whether the geometry shader has been attached to the window yet.
    attached: bool,
    /// Trails the window's top-left corner.
    lo: Spring,
    /// Trails the window's bottom-right corner.
    hi: Spring,
    max_lag: f32,
}

impl Wobble {
    fn new(window: Window, animation_id: u32, (lo, hi): (Vec2, Vec2)) -> Self {
        let smaller_side = window.size.width.min(window.size.height) as f32;
        Self {
            window,
            animation_id,
            attached: false,
            lo: Spring::new(lo),
            hi: Spring::new(hi),
            max_lag: MAX_LAG.min(smaller_side * MAX_LAG_FRACTION),
        }
    }

    fn set_target(&mut self, (lo, hi): (Vec2, Vec2)) {
        self.lo.target = lo;
        self.hi.target = hi;
    }

    fn step(&mut self, dt: f32) {
        self.lo.step(dt, self.max_lag);
        self.hi.step(dt, self.max_lag);
    }

    fn is_at_rest(&self) -> bool {
        self.lo.is_at_rest() && self.hi.is_at_rest()
    }

    fn end_tick(&mut self) {
        self.lo.previous_target = self.lo.target;
        self.hi.previous_target = self.hi.target;
    }
}

#[derive(Default)]
struct State {
    /// The registered geometry shader; `Some(None)` if registration failed.
    shader: Option<Option<u8>>,
    wobbles: HashMap<u64, Wobble>,
}

impl State {
    fn shader(&mut self) -> Option<u8> {
        *self
            .shader
            .get_or_insert_with(|| register_window_geometry_shader(WOBBLE_GEOMETRY_SHADER))
    }

    /// Detaches the wobble from `window_id` and stops ticking its springs.
    fn stop(&mut self, window_id: u64) {
        if let Some(wobble) = self.wobbles.remove(&window_id) {
            let _ = wobble.window.clear_shader_uniform(None);
            let _ = wobble.window.set_geometry_shader(None);
            let _ = cancel_custom_animation(wobble.animation_id);
        }
    }
}

thread_local! {
    // Plugins run single-threaded inside the WASM sandbox. The custom animation
    // callbacks are `'static`, so the wobbles live here rather than on the plugin.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Ticks the springs of `window_id`; called every frame by its custom animation.
fn tick(window_id: u64, dt: f32, elapsed_seconds: f32) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(shader) = state.shader() else {
            state.stop(window_id);
            return;
        };
        let Some(wobble) = state.wobbles.get_mut(&window_id) else {
            return;
        };

        // Host calls that act on the window are made here rather than in the move
        // hook, as this runs on the window manager's thread.
        if !wobble.attached {
            wobble.attached = wobble.window.set_geometry_shader(Some(shader)).is_ok();
        }

        wobble.step(dt);
        if wobble.is_at_rest() || elapsed_seconds >= MAX_WOBBLE_SECONDS {
            state.stop(window_id);
            return;
        }

        wobble.end_tick();
        let lo = wobble.lo.lag();
        let hi = wobble.hi.lag();
        let _ = wobble
            .window
            .set_shader_uniform("u_lag_lo", ShaderUniform::Vec2([lo.x, lo.y]));
        let _ = wobble
            .window
            .set_shader_uniform("u_lag_hi", ShaderUniform::Vec2([hi.x, hi.y]));
    });
}

#[derive(Default)]
struct WobblyWindows;

impl Plugin for WobblyWindows {
    fn window_move_animation(
        &mut self,
        data: &AnimationFrameData,
        window: &Window,
    ) -> Option<AnimationFrameResult> {
        // Follow wherever the built-in animation puts the window this frame. The
        // clip area is what is actually shown: while the window grows, its area
        // already has the final size and only the clip area grows with it.
        let builtin = data.builtin?;
        let rect = if builtin.completed {
            data.destination
        } else {
            builtin.clip_area.or(builtin.area)?
        };
        let target = (
            Vec2::new(rect.x, rect.y),
            Vec2::new(rect.x + rect.width, rect.y + rect.height),
        );

        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.shader()?;

            let window_id = window.id();
            if let Some(wobble) = state.wobbles.get_mut(&window_id) {
                wobble.set_target(target);
                return Some(());
            }

            let animation_id = queue_custom_animation(
                move |_, dt, elapsed_seconds| tick(window_id, dt, elapsed_seconds),
                MAX_WOBBLE_SECONDS + 1.0,
            )?;
            state
                .wobbles
                .insert(window_id, Wobble::new(window.clone(), animation_id, target));
            Some(())
        });

        // Leave the frame itself to the built-in animation (or another plugin).
        None
    }

    fn window_deleted(&mut self, window: &Window) {
        STATE.with(|state| state.borrow_mut().stop(window.id()));
    }
}

miracle_plugin::miracle_plugin!(WobblyWindows);
