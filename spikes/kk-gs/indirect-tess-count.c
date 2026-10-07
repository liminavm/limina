/*
 * Counts vertex shader invocations of a tessellated draw, direct and indirect.
 *
 * KosmicKrisp runs the vertex shader of a tessellated draw as compute. The
 * count must equal the vertex count whichever way the draw is issued; an
 * indirect draw that over-dispatches the compute grid shows up here as extra
 * invocations. Build inside a piglit tree (see README.md).
 */

#include "piglit-util-gl.h"

PIGLIT_GL_TEST_CONFIG_BEGIN

	config.supports_gl_core_version = 43;
	config.window_visual = PIGLIT_GL_VISUAL_RGBA | PIGLIT_GL_VISUAL_DOUBLE;
	config.khr_no_error_support = PIGLIT_NO_ERRORS;

PIGLIT_GL_TEST_CONFIG_END

static const char *vs_src =
	"#version 430\n"
	"layout(std430, binding = 0) buffer counter { uint invocations; };\n"
	"void main() {\n"
	"	atomicAdd(invocations, 1u);\n"
	"	gl_Position = vec4(float(gl_VertexID % 3) * 0.01, 0.0, 0.0, 1.0);\n"
	"}\n";

static const char *tcs_src =
	"#version 430\n"
	"layout(vertices = 3) out;\n"
	"void main() {\n"
	"	gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
	"	gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
	"	gl_TessLevelOuter[2] = 1.0; gl_TessLevelInner[0] = 1.0;\n"
	"}\n";

static const char *tes_src =
	"#version 430\n"
	"layout(triangles) in;\n"
	"void main() {\n"
	"	gl_Position = gl_in[0].gl_Position * gl_TessCoord.x +\n"
	"	              gl_in[1].gl_Position * gl_TessCoord.y +\n"
	"	              gl_in[2].gl_Position * gl_TessCoord.z;\n"
	"}\n";

static const char *fs_src =
	"#version 430\n"
	"out vec4 color;\n"
	"void main() { color = vec4(0.0, 1.0, 0.0, 1.0); }\n";

#define VERTICES (3 * 37)

static GLuint prog, ssbo, vao, indirect;

static GLuint
count_invocations(bool use_indirect)
{
	const GLuint zero = 0;
	glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(zero), &zero);

	if (use_indirect)
		glDrawArraysIndirect(GL_PATCHES, NULL);
	else
		glDrawArrays(GL_PATCHES, 0, VERTICES);

	glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
	GLuint n = 0;
	glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(n), &n);
	return n;
}

enum piglit_result
piglit_display(void)
{
	bool pass = true;

	GLuint direct = count_invocations(false);
	GLuint indirect_n = count_invocations(true);

	printf("vertices %u: direct %u invocations, indirect %u\n",
	       VERTICES, direct, indirect_n);

	/* The GL allows a vertex to be shaded more than once, but not fewer
	 * times; KosmicKrisp shades each exactly once when the grid is right. */
	pass = direct == VERTICES && indirect_n == VERTICES;
	pass = piglit_check_gl_error(GL_NO_ERROR) && pass;

	return pass ? PIGLIT_PASS : PIGLIT_FAIL;
}

void
piglit_init(int argc, char **argv)
{
	prog = piglit_build_simple_program_multiple_shaders(
		GL_VERTEX_SHADER, vs_src,
		GL_TESS_CONTROL_SHADER, tcs_src,
		GL_TESS_EVALUATION_SHADER, tes_src,
		GL_FRAGMENT_SHADER, fs_src,
		0);
	glUseProgram(prog);
	glPatchParameteri(GL_PATCH_VERTICES, 3);

	glGenVertexArrays(1, &vao);
	glBindVertexArray(vao);

	glGenBuffers(1, &ssbo);
	glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
	glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(GLuint), NULL,
		     GL_DYNAMIC_READ);
	glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);

	const GLuint cmd[4] = {VERTICES, 1, 0, 0};
	glGenBuffers(1, &indirect);
	glBindBuffer(GL_DRAW_INDIRECT_BUFFER, indirect);
	glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(cmd), cmd, GL_STATIC_DRAW);
}
