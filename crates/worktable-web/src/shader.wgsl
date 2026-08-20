@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    // Full-screen triangle with Worktable gradient
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0)
    );
    return vec4<f32>(pos[vertex_index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // Cheap radial gradient + vignette — proves WebGPU is live
    let uv = pos.xy / vec2<f32>(800.0, 220.0);
    let d = distance(uv, vec2<f32>(0.18, 0.52));
    let t = clamp(1.0 - d * 1.35, 0.0, 1.0);
    let base = vec3<f32>(0.06, 0.09, 0.16);
    let accent = vec3<f32>(0.22, 0.74, 0.97);
    let accent2 = vec3<f32>(0.99, 0.42, 0.22);
    var col = mix(base, accent, t * 0.85);
    col = mix(col, accent2, pow(t, 6.0) * 0.35);
    // vignette
    let vig = 1.0 - length((uv - vec2<f32>(0.5, 0.5)) * vec2<f32>(1.15, 0.85)) * 0.55;
    col *= clamp(vig, 0.72, 1.0);
    // subtle scanline
    let scan = 0.96 + 0.04 * sin(uv.y * 520.0);
    col *= scan;
    return vec4<f32>(col, 1.0);
}
