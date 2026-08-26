// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Mimic, with no compositor and no toolkit, the exact draw that loses a notification card's
// title -- so the fault can be chased on the HOST, where Metal tracing works.
//
// `rtsample.c` is the negative control that got us here: it walked one ingredient at a time and
// every arm came back 256/256 clean. The lesson recorded in RESULTS.md is that guessing
// ingredients singly is the wrong move, so this vehicle assembles EVERY measured property of the
// failing episode at once and minimises only after it reproduces.
//
// Every property below was extracted from a real failing draw in the vrend command trace
// (LIMINA_VREND_TRACE + vrend-trace-decode.py --fingerprint), not inferred, and all of it is
// encoded in the default arm:
//
//   - A card is NOT one label. It is a set of sibling offscreens -- 568x44 title, 968x44,
//     110x38, 134x44, 38x38 -- each drawn ONCE per frame and composited into the 2560x1440
//     stage. The title offscreen is the one that loses its text.
//   - A card lives exactly TWO frames, 13-36 ms apart, and is then abandoned; the next card
//     allocates fresh resources.
//   - Every offscreen carries a D24S8 depth-stencil attachment (S8_UINT_Z24_UNORM), and BOTH
//     attachment surface objects are freshly created each frame over persistent textures.
//   - The title is drawn by the clutter DISPLAY-LIST glyph shape: `CoglVertexP2T2
//     { float x, y, s, t }` -- vec2 position at offset 0, vec2 texcoord at offset 8, stride 16 --
//     with the colour from a stride-0 R32G32B32A32 binding fed by a separate long-lived buffer at
//     an offset advancing 16 bytes per frame. Strides 16 / 0 / 16.
//   - That stride-0 binding is produced by a vertex attribute whose ARRAY IS DISABLED, its value
//     supplied by glVertexAttrib4f. Mesa's state tracker turns a disabled array's current value
//     into a zero-stride vertex buffer at exactly that shape, separate resource and rolling
//     offset included -- it needs no building. A stride-0 glVertexAttribPointer is NOT the same
//     thing (GL stride 0 means tightly packed) and a uniform is a different pipeline entirely.
//     Getting this encoding wrong is the most likely way to build a mimic that cannot reproduce.
//   - DRAW_VBO [start 0, count 228, mode 4 (TRIANGLES), indexed, 1 instance] from a long-lived
//     uint8 index buffer at offset 0 that is never re-uploaded. 228 indices = 38 quads = 152
//     vertices, so uint8 indices come nowhere near their 255 ceiling: that ceiling is NOT a
//     property of this bug.
//   - The vertex upload is 2432 BYTES -- exactly those 152 vertices at stride 16, no slack. A
//     virgl buffer transfer's extent is in bytes, not dwords.
//   - The siblings are drawn by the cogl JOURNAL shape: vec3 position + R8G8B8A8_UNORM colour +
//     vec2 uv, binding strides 32 / 32 / 32.
//   - The glyph draw samples an alpha atlas, blended, and is correctly scissored and viewported;
//     an occlusion query over it reports samples=0.
//
// Verification follows rtsample's discipline, because it is what made that vehicle trustworthy:
// NO synchronisation anywhere in the loop (a readback or a finish would itself supply the batch
// boundary under test), ONE deferred readback after every episode, and occlusion-query results
// collected only at the end. GM_FINISH is the self-test arm -- it must score clean, which is what
// proves the oracle is able to say "cured" at all.
//
// Builds for the guest (virgl) and natively on the host (zink-on-KosmicKrisp); see mimic-build.sh.
//
// WHAT THIS VEHICLE IS AND IS NOT FOR. The LOCUS IS ALREADY SETTLED: the host-implementation split
// convicted zink/KosmicKrisp (same guest, same virgl stream, same vrend, only the host GL swapped:
// zink-on-KK 4 damaged / 4 clean, llvmpipe 0 damaged / 16 clean). The fault is below vrend. This
// vehicle is NOT re-litigating that, and a clean host run is NOT evidence for KK's innocence.
//
// The open question is the TRIGGER, not the locus: which input provokes zink/KK. That matters
// because the GL this file writes by hand is not the GL zink/KK actually receives in the real case
// -- there, vrend EMITS the GL from the guest's virgl commands, with its own state setting, buffer
// orphaning and bind pattern. So:
//   host reproduces  -> the trigger is contained in cogl's shape; iterate here, with Metal tooling
//                       and no VM (Apple's capture layer segfaults on the VM's command stream).
//   guest reproduces + host clean -> the trigger is in what VREND emits, and the next step is to
//                       diff vrend's real GL against this file's, not to guess more cogl-side.
//   both clean       -> the fingerprint is incomplete whichever path feeds it. Measure vrend's
//                       actual call stream and replay that; do not add more guessed arms.
//
//   glyphmimic [episodes]        (an "episode" is one notification card)
//
// The DEFAULT arm is the faithful one: the card exactly as the vrend trace measures it. Do not
// weaken it to "simplify" -- every property here was extracted from a real failing draw, and the
// history of this file is that additive guessing never provoked anything.
//
// Arms are SUBTRACTIVE. Each removes one measured ingredient, so that if the faithful arm ever
// does reproduce, the reproduction can be minimised by finding which removal cures it:
//   GM_NODEPTH=1   drop the D24S8 depth-stencil attachment. The real offscreens all carry one
//                  (VIRGL_FORMAT_S8_UINT_Z24_UNORM); Metal has no 24-bit depth, so KosmicKrisp
//                  EMULATES this format -- and D24S8 emulation is one of the exonerations from
//                  the run of five pixel-identical A/B results, i.e. it was never under test.
//   GM_FRAMES=n    frames per card (default 2, measured: 10 cards, 20 glyph draws, 2 per colour
//                  resource, 13-36 ms apart, then the resources are abandoned)
//   GM_WIDE=1      give every sibling offscreen the title's width, collapsing the size mix
//   GM_COMPOSITES=n  stage composites per frame (default 13). At 0 nothing reaches the stage and
//                  the verdict is BLANK -- that is the readback going away, not a cure.
//   GM_GAP_MS=n    ms between a card's frames (default 20)
//   GM_NOSTRIDE0=1 give the colour a real per-vertex array instead of the constant attribute
//                  -- the discriminator for the stride-0 hypothesis
//   GM_U16=1       GL_UNSIGNED_SHORT indices instead of uint8
//   GM_W, GM_H     title offscreen dimensions (default 568x44, the measured size)
//   GM_FINISH=1    glFinish after the glyph pass          -- the self-test; MUST be clean
//   GM_FLUSH=1     glFlush after the glyph pass           -- the predicted cure
//   GM_PRESENT=1   glFlush at the end of each FRAME. This is what puts a frame's transfers in the
//                  same virgl batch as the draws that read them: without it virgl accumulates
//                  many frames into one batch and emits every transfer at its head, hundreds of
//                  ms from its consumer. The shell holds zero batch boundaries between an upload
//                  and its draw on every single one.
//   GM_NODRAW=1    OMIT the glyph draw entirely -- the POSITIVE CONTROL. This is not an arm, it
//                  is the proof that the oracle can say "text lost" at all. Every real arm here
//                  scoring clean is only evidence if a run that genuinely loses the text scores
//                  all-lost, and an ink detector that always fires would look identical to a
//                  cure. It must report text-lost on every episode, and it must be re-proven on
//                  each platform: the host proof does not transfer to the guest's readback path.
//
// Keep two things OUT of the window between the upload burst and the glyph draw, or they split it
// across a batch boundary themselves and destroy the property under test: glCheckFramebufferStatus
// (a round trip -- check the FBO once, not per frame) and any readback.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAIL %s @%d\n", #x, __LINE__); exit(2); } } while (0)

// All four numbers come from the failing DRAW_VBO in the vrend trace, not from a guess:
//   DRAW_VBO [start 0, count 228, mode 4 (TRIANGLES), indexed 1, instances 1]
// 228 indices is 38 quads and 152 vertices, so uint8 indices never come near their 255 ceiling --
// the ceiling is NOT a property of this bug, and an earlier note in this file claiming it was
// measured was an artifact of a 64-quad choice made before the trace existed.
#define QUADS   38            // 228 indices / 6
#define GVERTS  (QUADS * 4)   // 152 vertices, max index 151
#define GIDX    (QUADS * 6)   // 228, as measured
// The upload is exactly the draw's data, with no slack: TRANSFER res=354 box 2432x1. A virgl
// buffer transfer's extent is in BYTES (confirmed against this mimic's own trace, which reports
// back precisely the byte count it uploaded), so 2432 / 16 = 152 vertices = GVERTS.
#define GBUF_VERTS GVERTS
#define BGQUADS 18            // the measured journal entry count for a sibling offscreen

// Pass 1 -- the cogl journal shape: vec3 position, packed RGBA8 colour, vec2 uv, stride 32.
static const char *VS_JOURNAL =
    "#version 300 es\n"
    "layout(location = 0) in vec3 pos;\n"
    "layout(location = 1) in vec4 col;\n"
    "layout(location = 2) in vec2 uv;\n"
    "out vec4 vcol; out vec2 vuv;\n"
    "void main(){ vcol = col; vuv = uv; gl_Position = vec4(pos, 1.0); }\n";
static const char *FS_JOURNAL =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform sampler2D atlas;\n"
    "in vec4 vcol; in vec2 vuv; out vec4 c;\n"
    // The journal samples a texture too. This must genuinely CONSUME vuv: a journal shader that
    // ignores it gets the uv attribute dead-stripped, and the pass then compiles to two
    // attributes instead of the measured three. The uvs address the atlas's opaque interior, so
    // the multiply is by 1.0 and the background colour is unchanged.
    "void main(){ c = vcol * texture(atlas, vuv).r; }\n";

// Pass 2 -- the clutter display-list glyph shape. `col` is deliberately read but NOT fed from an
// enabled array in the default arm: glVertexAttrib4f supplies it, which is what becomes the
// stride-0 R32G32B32A32 binding in the Vulkan pipeline.
static const char *VS_GLYPH =
    "#version 300 es\n"
    "layout(location = 0) in vec2 pos;\n"
    "layout(location = 1) in vec4 col;\n"
    "layout(location = 2) in vec2 uv;\n"
    "out vec4 vcol; out vec2 vuv;\n"
    "void main(){ vcol = col; vuv = uv; gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *FS_GLYPH =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform sampler2D atlas;\n"
    "in vec4 vcol; in vec2 vuv; out vec4 c;\n"
    "void main(){ c = vec4(vcol.rgb, vcol.a * texture(atlas, vuv).r); }\n";

// The stage composite: a full-target triangle from gl_VertexID, sampling the label offscreen.
static const char *VS_BLIT =
    "#version 300 es\n"
    "out vec2 uv;\n"
    "void main(){ vec2 p = vec2(float((gl_VertexID<<1)&2), float(gl_VertexID&2));\n"
    "  uv = p; gl_Position = vec4(p*2.0-1.0, 0.0, 1.0); }\n";
static const char *FS_BLIT =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform sampler2D t; in vec2 uv; out vec4 c;\n"
    "void main(){ c = texture(t, uv); }\n";

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); exit(2); }
    return s;
}

static GLuint link_prog(const char *vs_src, const char *fs_src) {
    GLuint p = glCreateProgram();
    glAttachShader(p, compile(GL_VERTEX_SHADER, vs_src));
    glAttachShader(p, compile(GL_FRAGMENT_SHADER, fs_src));
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[2048]; glGetProgramInfoLog(p, sizeof log, NULL, log); fprintf(stderr, "link: %s\n", log); exit(2); }
    return p;
}

static float ndc(float v, float extent) { return (v / extent) * 2.0f - 1.0f; }

int main(int argc, char **argv) {
    int episodes = argc > 1 ? atoi(argv[1]) : 32;
    int ow = getenv("GM_W") ? atoi(getenv("GM_W")) : 568;
    int oh = getenv("GM_H") ? atoi(getenv("GM_H")) : 44;
    int a_finish   = getenv("GM_FINISH") != NULL;
    int a_flush    = getenv("GM_FLUSH") != NULL;
    int a_present  = getenv("GM_PRESENT") != NULL;
    int a_nostride0= getenv("GM_NOSTRIDE0") != NULL;
    int a_u16      = getenv("GM_U16") != NULL;
    int a_nodraw   = getenv("GM_NODRAW") != NULL;
    // Subtractive arms over the faithful default: each removes one measured ingredient, so a
    // reproduction can be minimised by finding which removal cures it.
    int a_nodepth  = getenv("GM_NODEPTH") != NULL;          // drop the D24S8 attachment
    int a_wide     = getenv("GM_WIDE") != NULL;             // every offscreen at the title's width
    int a_frames   = getenv("GM_FRAMES") ? atoi(getenv("GM_FRAMES")) : 2;
    int a_composites = getenv("GM_COMPOSITES") ? atoi(getenv("GM_COMPOSITES")) : 13;
    int a_gap_us   = (getenv("GM_GAP_MS") ? atoi(getenv("GM_GAP_MS")) : 20) * 1000;

    // The stage stacks each episode's label 1:1 in its own row, so ink is detectable per episode
    // rather than smeared into a downscaled cell.
    if (episodes * oh > 4096) episodes = 4096 / oh;
    int sw = ow, sh = episodes * oh;

    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    CHECK(dpy != EGL_NO_DISPLAY);
    CHECK(eglInitialize(dpy, NULL, NULL));
    CHECK(eglBindAPI(EGL_OPENGL_ES_API));
    EGLint cfgattr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE };
    EGLConfig cfg; EGLint n = 0;
    CHECK(eglChooseConfig(dpy, cfgattr, &cfg, 1, &n) && n > 0);
    EGLint ctxattr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxattr);
    CHECK(ctx != EGL_NO_CONTEXT);
    CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));

    printf("glyphmimic: %s | %d cards, title %dx%d, stage %dx%d | arms:%s%s%s%s%s%s%s\n",
           (const char *)glGetString(GL_RENDERER), episodes, ow, oh, sw, sh,
           a_finish ? " FINISH" : "", a_flush ? " FLUSH" : "", a_present ? " PRESENT" : "",
           a_nostride0 ? " NOSTRIDE0" : "", a_u16 ? " U16" : "",
           a_nodepth ? " NODEPTH" : "", a_wide ? " WIDE" : "");
    printf("glyphmimic: %d frames per card, %d composites per frame, %d ms between frames, "
           "%s depth\n", a_frames, a_composites, a_gap_us / 1000,
           a_nodepth ? "NO" : "D24S8");
    if (a_nodraw) printf("glyphmimic: POSITIVE CONTROL -- glyph draw omitted; expect text-lost on every card\n");
    printf("glyphmimic: glyph pass = %d quads, %d verts, %d %s indices, colour %s\n",
           QUADS, GVERTS, GIDX, a_u16 ? "uint16" : "uint8",
           a_nostride0 ? "from a per-vertex ARRAY" : "from glVertexAttrib4f (stride-0 binding)");

    GLuint p_journal = link_prog(VS_JOURNAL, FS_JOURNAL);
    GLuint p_glyph   = link_prog(VS_GLYPH, FS_GLYPH);
    GLuint p_blit    = link_prog(VS_BLIT, FS_BLIT);
    glUseProgram(p_glyph);
    glUniform1i(glGetUniformLocation(p_glyph, "atlas"), 0);
    glUseProgram(p_journal);
    glUniform1i(glGetUniformLocation(p_journal, "atlas"), 0);

    // ---- the glyph atlas: alpha-only, as a real glyph atlas is. Opaque in the middle so every
    // glyph quad is guaranteed ink; a lost draw is then unambiguous.
    unsigned char *ap = malloc(256 * 256);
    CHECK(ap);
    for (int y = 0; y < 256; y++)
        for (int x = 0; x < 256; x++)
            ap[y * 256 + x] = (x >= 48 && x < 208 && y >= 48 && y < 208) ? 255 : 0;
    GLuint atlas;
    glGenTextures(1, &atlas);
    glBindTexture(GL_TEXTURE_2D, atlas);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, 256, 256, 0, GL_RED, GL_UNSIGNED_BYTE, ap);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    free(ap);

    // ---- pass 1 geometry: BGQUADS tiles covering the label, journal layout, stride 32.
    //      vec3 position @0, RGBA8 colour @12, vec2 uv @16, 8 bytes padding to 32.
    unsigned char *jverts = calloc(BGQUADS * 4, 32);
    CHECK(jverts);
    for (int q = 0; q < BGQUADS; q++) {
        float x0 = (float)q * ow / BGQUADS, x1 = (float)(q + 1) * ow / BGQUADS;
        float corner[4][2] = { {x0,0}, {x1,0}, {x0,(float)oh}, {x1,(float)oh} };
        for (int v = 0; v < 4; v++) {
            unsigned char *b = jverts + ((size_t)q * 4 + v) * 32;
            float *f = (float *)b;
            f[0] = ndc(corner[v][0], (float)ow);
            f[1] = ndc(corner[v][1], (float)oh);
            f[2] = 0.0f;
            b[12] = 0; b[13] = 0; b[14] = 255; b[15] = 255;   // the card background, BLUE
            float *uv = (float *)(b + 16);
            uv[0] = (v & 1) ? 0.70f : 0.30f;
            uv[1] = (v & 2) ? 0.70f : 0.30f;
        }
    }

    // ---- pass 2 geometry: CoglVertexP2T2 -- vec2 position @0, vec2 uv @8, stride 16.
    // Allocated at the UPLOAD size, not the draw size: the frame writes all 608 vertices and the
    // draw consumes 152 of them, as the trace shows.
    float *gbuf = calloc(GBUF_VERTS, sizeof(float) * 4);
    CHECK(gbuf);
    float *gverts = gbuf;
    for (int q = 0; q < QUADS; q++) {
        float gw = (float)ow / QUADS, x0 = q * gw + 1.0f, x1 = (q + 1) * gw - 1.0f;
        float y0 = oh * 0.25f, y1 = oh * 0.75f;
        float corner[4][2] = { {x0,y0}, {x1,y0}, {x0,y1}, {x1,y1} };
        for (int v = 0; v < 4; v++) {
            float *f = gverts + ((size_t)q * 4 + v) * 4;
            f[0] = ndc(corner[v][0], (float)ow);
            f[1] = ndc(corner[v][1], (float)oh);
            // Sample well inside the opaque region, so every glyph quad is solid ink.
            f[2] = (v & 1) ? 0.70f : 0.30f;
            f[3] = (v & 2) ? 0.70f : 0.30f;
        }
    }
    // The optional per-vertex colour array for GM_NOSTRIDE0, in its own buffer so the default
    // arm's interleaved stride-16 layout is untouched.
    float *gcols = calloc(GBUF_VERTS, sizeof(float) * 4);
    CHECK(gcols);
    for (int i = 0; i < GBUF_VERTS; i++) { gcols[i*4+0] = 1.0f; gcols[i*4+1] = 0.0f; gcols[i*4+2] = 0.0f; gcols[i*4+3] = 1.0f; }

    // ---- the CACHED rectangle index buffer, built once, as cogl caches its own.
    unsigned char  *idx8  = malloc(GIDX);
    unsigned short *idx16 = malloc(GIDX * sizeof(unsigned short));
    CHECK(idx8 && idx16);
    for (int q = 0; q < QUADS; q++) {
        int b = q * 4, o = q * 6;
        int seq[6] = { b, b+1, b+2, b+2, b+1, b+3 };
        for (int k = 0; k < 6; k++) { idx8[o+k] = (unsigned char)seq[k]; idx16[o+k] = (unsigned short)seq[k]; }
    }
    GLuint ibo, jibo, jvbo, gvbo, gcbo;
    glGenBuffers(1, &ibo);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
    if (a_u16) glBufferData(GL_ELEMENT_ARRAY_BUFFER, GIDX * sizeof(unsigned short), idx16, GL_STATIC_DRAW);
    else       glBufferData(GL_ELEMENT_ARRAY_BUFFER, GIDX, idx8, GL_STATIC_DRAW);
    // Pass 1's own indices (BGQUADS quads).
    unsigned char *jidx = malloc(BGQUADS * 6);
    CHECK(jidx);
    for (int q = 0; q < BGQUADS; q++) {
        int b = q * 4, o = q * 6;
        int seq[6] = { b, b+1, b+2, b+2, b+1, b+3 };
        for (int k = 0; k < 6; k++) jidx[o+k] = (unsigned char)seq[k];
    }
    glGenBuffers(1, &jibo);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, jibo);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, BGQUADS * 6, jidx, GL_STATIC_DRAW);

    glGenBuffers(1, &jvbo);
    glBindBuffer(GL_ARRAY_BUFFER, jvbo);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)BGQUADS * 4 * 32, NULL, GL_DYNAMIC_DRAW);
    glGenBuffers(1, &gvbo);
    glGenBuffers(1, &gcbo);
    // Sized for the whole upload; every frame rewrites it with glBufferSubData, never
    // glBufferData -- respecifying would orphan the buffer and mint a fresh resource per frame,
    // which is not the shape the shell has (its res 354 is written in place).
    glBindBuffer(GL_ARRAY_BUFFER, gvbo);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)GBUF_VERTS * 16, gbuf, GL_DYNAMIC_DRAW);
    glBindBuffer(GL_ARRAY_BUFFER, gcbo);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)GBUF_VERTS * 16, gcols, GL_STATIC_DRAW);

    // ---- the stage, persistent, cleared to transparent so "nothing arrived" is distinguishable.
    GLuint stex, sfbo;
    glGenTextures(1, &stex);
    glBindTexture(GL_TEXTURE_2D, stex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, sw, sh, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &sfbo);
    glBindFramebuffer(GL_FRAMEBUFFER, sfbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, stex, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    glClearColor(0, 0, 0, 0);
    glClear(GL_COLOR_BUFFER_BIT);

    // ---- The card, as the trace measures it. A notification card is not one label: it is a set
    // of sibling offscreens, each holding one piece, each drawn ONCE per compositor frame and
    // then composited into the stage. A card lives exactly TWO frames (measured: 10 cards, 20
    // glyph draws, 2 per colour resource, 13-36 ms apart) and is then abandoned -- the next card
    // gets fresh resources.
    //
    // Three properties here were measured late and each is load-bearing:
    //   * every offscreen carries a D24S8 DEPTH-STENCIL attachment (fmt S8_UINT_Z24_UNORM).
    //     Metal has no 24-bit depth, so KosmicKrisp emulates this format -- and "D24S8 emu" is
    //     one of the exonerations from the run of five identical A/B results, i.e. it was never
    //     actually tested.
    //   * the FBO is FRESH every frame while the textures persist. In the trace both attachment
    //     surface objects are newly created for each paint and wrap the same two resources.
    //   * the frame's whole vertex upload arrives in one burst immediately before its draws, with
    //     no submit boundary in between.
    struct off { int w, h; GLuint tex, dep; int glyph; };
    static const struct { int w, h; int glyph; } CARD[] = {
        { 568,  44, 1 },   // the title -- the offscreen that loses its text
        { 968,  44, 0 },
        { 110,  38, 0 },
        { 134,  44, 0 },
        {  38,  38, 0 },
    };
    const int NOFF = (int)(sizeof CARD / sizeof CARD[0]);

    GLuint *qry = calloc(episodes, sizeof *qry);
    struct off *off = calloc(NOFF, sizeof *off);
    CHECK(qry && off);
    glGenQueries(episodes, qry);

    for (int i = 0; i < episodes; i++) {
        // Fresh resources per card. Colour AND depth persist across the card's two frames.
        for (int o = 0; o < NOFF; o++) {
            off[o].w = a_wide ? ow : CARD[o].w;
            off[o].h = CARD[o].h;
            off[o].glyph = CARD[o].glyph;
            glGenTextures(1, &off[o].tex);
            glBindTexture(GL_TEXTURE_2D, off[o].tex);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, off[o].w, off[o].h, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            if (!a_nodepth) {
                glGenTextures(1, &off[o].dep);
                glBindTexture(GL_TEXTURE_2D, off[o].dep);
                glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH24_STENCIL8, off[o].w, off[o].h, 0,
                             GL_DEPTH_STENCIL, GL_UNSIGNED_INT_24_8, NULL);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            }
        }

        for (int frame = 0; frame < a_frames; frame++) {
            // ---- the frame's upload burst: every buffer this frame draws from, back to back,
            // with no intervening draw or flush. This is what puts zero submit boundaries
            // between an upload and the draw that consumes it.
            glBindBuffer(GL_ARRAY_BUFFER, gvbo);
            glBufferSubData(GL_ARRAY_BUFFER, 0, (GLsizeiptr)GBUF_VERTS * 16, gbuf);
            glBindBuffer(GL_ARRAY_BUFFER, jvbo);
            glBufferSubData(GL_ARRAY_BUFFER, 0, (GLsizeiptr)BGQUADS * 4 * 32, jverts);

            // ---- one draw per offscreen, in the measured order, each into a FRESH fbo.
            glEnable(GL_SCISSOR_TEST);
            for (int o = 0; o < NOFF; o++) {
                GLuint fbo;
                glGenFramebuffers(1, &fbo);
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, off[o].tex, 0);
                if (!a_nodepth)
                    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_TEXTURE_2D, off[o].dep, 0);
                // Checked ONCE, on the first card only. glCheckFramebufferStatus is a round trip:
                // left in the loop it lands between the upload burst and the draw and splits them
                // across submit batches, destroying the one invariant the shell holds on every
                // single glyph draw (zero batch boundaries between upload and consumer).
                if (i == 0 && frame == 0)
                    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
                glViewport(0, 0, off[o].w, off[o].h);
                glScissor(0, 0, off[o].w, off[o].h);
                glClearColor(0, 0, 0, 0);
                glClear(GL_COLOR_BUFFER_BIT | (a_nodepth ? 0 : GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT));
                glActiveTexture(GL_TEXTURE0);
                glBindTexture(GL_TEXTURE_2D, atlas);

                if (off[o].glyph) {
                    // ---- the failing draw: 228 uint8 indices off the front of a 2432-dword
                    // upload, colour from the disabled array's current value.
                    glEnable(GL_BLEND);
                    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
                    glUseProgram(p_glyph);
                    glBindBuffer(GL_ARRAY_BUFFER, gvbo);
                    glEnableVertexAttribArray(0);
                    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, (void *)0);
                    glEnableVertexAttribArray(2);
                    glVertexAttribPointer(2, 2, GL_FLOAT, GL_FALSE, 16, (void *)8);
                    if (a_nostride0) {
                        glBindBuffer(GL_ARRAY_BUFFER, gcbo);
                        glEnableVertexAttribArray(1);
                        glVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 16, (void *)0);
                    } else {
                        // THE ingredient. The array stays DISABLED and the value comes from the
                        // current vertex attribute, which mesa lowers to a zero-stride vertex
                        // binding fed from a separate buffer at a rolling offset -- the shape the
                        // trace shows the shell using. Do not "simplify" this into a uniform.
                        glDisableVertexAttribArray(1);
                        glVertexAttrib4f(1, 1.0f, 0.0f, 0.0f, 1.0f);   // the title colour, RED
                    }
                    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
                    if (frame == a_frames - 1) glBeginQuery(GL_ANY_SAMPLES_PASSED, qry[i]);
                    if (!a_nodraw)
                        glDrawElements(GL_TRIANGLES, GIDX, a_u16 ? GL_UNSIGNED_SHORT : GL_UNSIGNED_BYTE, (void *)0);
                    if (frame == a_frames - 1) glEndQuery(GL_ANY_SAMPLES_PASSED);
                    glDisableVertexAttribArray(0);
                    glDisableVertexAttribArray(1);
                    glDisableVertexAttribArray(2);
                    glDisable(GL_BLEND);
                } else {
                    // ---- a sibling piece, drawn by the journal pipeline.
                    glDisable(GL_BLEND);
                    glUseProgram(p_journal);
                    glBindBuffer(GL_ARRAY_BUFFER, jvbo);
                    glEnableVertexAttribArray(0);
                    glVertexAttribPointer(0, 3, GL_FLOAT, GL_FALSE, 32, (void *)0);
                    glEnableVertexAttribArray(1);
                    glVertexAttribPointer(1, 4, GL_UNSIGNED_BYTE, GL_TRUE, 32, (void *)12);
                    glEnableVertexAttribArray(2);
                    glVertexAttribPointer(2, 2, GL_FLOAT, GL_FALSE, 32, (void *)16);
                    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, jibo);
                    glDrawElements(GL_TRIANGLES, BGQUADS * 6, GL_UNSIGNED_BYTE, (void *)0);
                    glDisableVertexAttribArray(0);
                    glDisableVertexAttribArray(1);
                    glDisableVertexAttribArray(2);
                }
                // The surface object dies with the fbo, which is what makes the next frame's
                // attachment surfaces fresh.
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                glDeleteFramebuffers(1, &fbo);
            }

            if (a_finish) glFinish();
            else if (a_flush) glFlush();

            // ---- the composite. The shell puts ~13 draws into its 2560x1440 stage per frame;
            // only the first carries this card's title, the rest are volume that lands between
            // this frame's glyph draw and the next.
            glBindFramebuffer(GL_FRAMEBUFFER, sfbo);
            glEnable(GL_BLEND);
            glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            glUseProgram(p_blit);
            for (int c = 0; c < a_composites; c++) {
                int o = c % NOFF;
                glViewport(0, i * oh, o == 0 ? ow : 8, oh);
                glScissor(0, i * oh, o == 0 ? ow : 8, oh);
                glBindTexture(GL_TEXTURE_2D, off[o].tex);
                glDrawArrays(GL_TRIANGLES, 0, 3);
            }
            glDisable(GL_BLEND);
            glDisable(GL_SCISSOR_TEST);

            if (a_present) glFlush();
            if (a_gap_us) usleep(a_gap_us);
        }

        // The card is abandoned; the next one allocates afresh. Deleting here is what makes the
        // resource ids advance the way the trace shows them advancing.
        for (int o = 0; o < NOFF; o++) {
            glDeleteTextures(1, &off[o].tex);
            if (off[o].dep) glDeleteTextures(1, &off[o].dep);
        }
    }


    // ONE readback, after everything -- doing it per episode would supply the very boundary
    // being tested for.
    unsigned char *px = malloc((size_t)sw * sh * 4);
    CHECK(px);
    glBindFramebuffer(GL_FRAMEBUFFER, sfbo);
    glReadPixels(0, 0, sw, sh, GL_RGBA, GL_UNSIGNED_BYTE, px);

    int ok = 0, textlost = 0, blank = 0, other = 0, zero_samples = 0;
    for (int i = 0; i < episodes; i++) {
        long ink = 0, bg = 0, empty = 0;
        for (int y = i * oh; y < (i + 1) * oh; y++) {
            for (int x = 0; x < sw; x++) {
                const unsigned char *p = px + (((size_t)y * sw) + x) * 4;
                if (p[0] > 180 && p[2] < 80) ink++;
                else if (p[2] > 180 && p[0] < 80) bg++;
                else if (p[3] < 16) empty++;
            }
        }
        GLuint any = 1;
        glGetQueryObjectuiv(qry[i], GL_QUERY_RESULT, &any);
        if (!any) zero_samples++;

        if (ink > 0 && bg > 0) ok++;
        else if (bg > 0 && ink == 0) {
            if (textlost < 6) printf("  episode %d TEXT LOST (background %ld px, ink 0, samples_passed=%u)\n", i, bg, any);
            textlost++;
        } else if (empty > (long)sw * oh / 2) {
            if (blank < 6) printf("  episode %d BLANK (nothing arrived)\n", i);
            blank++;
        } else {
            if (other < 6) printf("  episode %d OTHER ink=%ld bg=%ld empty=%ld\n", i, ink, bg, empty);
            other++;
        }
    }
    printf("VERDICT: ok=%d text-lost=%d blank=%d other=%d of %d | occlusion samples=0 on %d\n",
           ok, textlost, blank, other, episodes, zero_samples);
    return (ok == episodes) ? 0 : 1;
}
