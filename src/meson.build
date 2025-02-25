dep_xdamage = dependency('xdamage')
dep_xcomposite = dependency('xcomposite')
dep_xcursor = dependency('xcursor')
dep_xrender = dependency('xrender')
dep_xext = dependency('xext')
dep_xfixes = dependency('xfixes')
dep_xxf86vm = dependency('xxf86vm')
dep_xtst = dependency('xtst')
dep_xres = dependency('xres')
dep_xmu = dependency('xmu')
dep_xi = dependency('xi')

drm_dep = dependency('libdrm', version: '>= 2.4.113', required: get_option('drm_backend'))

wayland_server = dependency('wayland-server', version: '>=1.21')
wayland_protos = dependency('wayland-protocols', version: '>=1.17')
xkbcommon = dependency('xkbcommon')
thread_dep = dependency('threads')
cap_dep = dependency('libcap', required: get_option('rt_cap'))
epoll_dep = dependency('epoll-shim', required: false)
glm_dep = dependency('glm')
sdl_dep = dependency('SDL2', required: get_option('sdl2_backend'))
stb_dep = dependency('stb')
avif_dep = dependency('libavif', version: '>=1.0.0', required: get_option('avif_screenshots'))

wlroots_dep = dependency(
  'wlroots',
  version: ['>= 0.17.0', '< 0.18.0'],
  fallback: ['wlroots', 'wlroots'],
  default_options: ['default_library=static', 'examples=false', 'xwayland=enabled', 'backends=libinput', 'renderers=[]', 'allocators=[]', 'session=enabled'],
)

displayinfo_dep = dependency(
  'libdisplay-info',
  version: ['>= 0.0.0', '< 0.2.0'],
  fallback: ['libdisplay-info', 'di_dep'],
  default_options: ['default_library=static'],
)

libdecor_dep = dependency('libdecor-0')

glsl_compiler = find_program('glslang', 'glslangValidator', native: true)

# Use --depfile to rebuild shaders when included files have changed. Sadly debian based
# distros don't have up-to-date glslang so we need to check for support first.
if run_command(glsl_compiler, ['--version', '--depfile', 'dummy.dep'], check: false).returncode() == 0
  glsl_generator = generator(
    glsl_compiler,
    output    : ['@BASENAME@.h'],
    arguments : ['-V', '@INPUT@', '--vn', '@BASENAME@', '-o', '@OUTPUT@', '--depfile', '@DEPFILE@', '--quiet'],
    depfile   : '@BASENAME@.h.d',
  )
else
  glsl_generator = generator(
    glsl_compiler,
    output    : ['@BASENAME@.h'],
    arguments : ['-V', '@INPUT@', '--vn', '@BASENAME@', '-o', '@OUTPUT@'],
  )
endif

shader_src = [
  'shaders/cs_composite_blit.comp',
  'shaders/cs_composite_blur.comp',
  'shaders/cs_composite_blur_cond.comp',
  'shaders/cs_composite_rcas.comp',
  'shaders/cs_easu.comp',
  'shaders/cs_easu_fp16.comp',
  'shaders/cs_gaussian_blur_horizontal.comp',
  'shaders/cs_nis.comp',
  'shaders/cs_nis_fp16.comp',
  'shaders/cs_rgb_to_nv12.comp',
]

spirv_shaders = glsl_generator.process(shader_src)

reshade_src = [
  'reshade/source/effect_codegen_spirv.cpp',
  'reshade/source/effect_expression.cpp',
  'reshade/source/effect_lexer.cpp',
  'reshade/source/effect_parser_exp.cpp',
  'reshade/source/effect_parser_stmt.cpp',
  'reshade/source/effect_preprocessor.cpp',
  'reshade/source/effect_symbol_table.cpp',
]

reshade_include = include_directories([
  'reshade/source',
  'reshade/include',
  '../thirdparty/SPIRV-Headers/include/spirv/unified1'
])

required_wlroots_features = ['xwayland']

src = [
  'steamcompmgr.cpp',
  'convar.cpp',
  'color_helpers.cpp',
  'main.cpp',
  'edid.cpp',
  'headless.cpp',
  'wlserver.cpp',
  'vblankmanager.cpp',
  'rendervulkan.cpp',
  'log.cpp',
  'ime.cpp',
  'mangoapp.cpp',
  'reshade_effect_manager.cpp',
  'backend.cpp',
  'wayland_backend.cpp',
]

gamescope_cpp_args = []
if drm_dep.found()
  src += 'drm.cpp'
  src += 'modegen.cpp'
  required_wlroots_features += 'libinput_backend'
  liftoff_dep = dependency(
    'libliftoff',
    version: ['>= 0.4.0', '< 0.5.0'],
    fallback: ['libliftoff', 'liftoff'],
    default_options: ['default_library=static'],
  )
else
  liftoff_dep = dependency('', required: false)
endif

if sdl_dep.found()
  src += 'sdlwindow.cpp'
endif

gamescope_cpp_args += '-DHAVE_DRM=@0@'.format(drm_dep.found().to_int())
gamescope_cpp_args += '-DHAVE_SDL2=@0@'.format(sdl_dep.found().to_int())
gamescope_cpp_args += '-DHAVE_AVIF=@0@'.format(avif_dep.found().to_int())
gamescope_cpp_args += '-DHAVE_LIBCAP=@0@'.format(cap_dep.found().to_int())

src += spirv_shaders
src += protocols_server_src
src += protocols_client_src

if pipewire_dep.found()
  src += 'pipewire.cpp'
endif

if openvr_dep.found()
  src += 'vr_session.cpp'
endif

foreach feat : required_wlroots_features
  if wlroots_dep.get_variable('have_' + feat) != 'true'
    error('Cannot use wlroots built without ' + feat + ' support')
  endif
endforeach

  executable(
    'gamescope',
    src, reshade_src,
    include_directories : [reshade_include],
    dependencies: [
      dep_wayland, dep_x11, dep_xdamage, dep_xcomposite, dep_xrender, dep_xext, dep_xfixes,
      dep_xxf86vm, dep_xres, glm_dep, drm_dep, wayland_server,
      xkbcommon, thread_dep, sdl_dep, wlroots_dep,
      vulkan_dep, liftoff_dep, dep_xtst, dep_xmu, cap_dep, epoll_dep, pipewire_dep, librt_dep,
      stb_dep, displayinfo_dep, openvr_dep, dep_xcursor, avif_dep, dep_xi,
      libdecor_dep,
    ],
    install: true,
    cpp_args: gamescope_cpp_args,
  )


benchmark_dep = dependency('benchmark', required: get_option('benchmark'), disabler: true)
executable('gamescope_color_microbench', ['color_bench.cpp', 'color_helpers.cpp'], dependencies:[benchmark_dep, glm_dep])

executable('gamescope_color_tests', ['color_tests.cpp', 'color_helpers.cpp'], dependencies:[glm_dep])
