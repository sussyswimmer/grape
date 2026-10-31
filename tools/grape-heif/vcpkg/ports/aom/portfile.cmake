# Grape's overlay (DECISIONS D41): libaom 3.15.2 on the recipe of vcpkg
# e456309491fd875487bf02b8a0dca76c1b1b00cc (aom 3.15.1). libheif encodes AVIF
# with it; its decoder is compiled but never referenced (libheif's aom decoder
# is off), so the linker leaves it out of grape-heif.exe.
# The one change from that recipe: the source is AOM's release tarball,
# checked by its SHA-512 (vcpkg_from_git checks no hash, and the archive it
# makes with the local git varies with git's version), so the archive that was
# built is the one in the GPL source bundle. Perl, NASM, the options, the patch
# and the copyright list are the recipe's.
vcpkg_download_distfile(ARCHIVE
    URLS "https://storage.googleapis.com/aom-releases/libaom-${VERSION}.tar.gz"
    FILENAME "libaom-${VERSION}.tar.gz"
    SHA512 4ba185a642c9dd32d1ab708916a5aed1a65d3cd6d2599d632f33c83849e6702ca91528ad09a007eb39ac2723f0b10de9a7905ed26abfef42a164f13e21aacb7a
)
vcpkg_extract_source_archive(SOURCE_PATH
    ARCHIVE "${ARCHIVE}"
    PATCHES
        aom-rename-static.diff
)

vcpkg_find_acquire_program(PERL)

if(VCPKG_TARGET_ARCHITECTURE MATCHES "^(x86|x64)$")
    # Upstream AOM only uses NASM on x86-family targets. Non-x86 targets such as
    # Apple Silicon configure with ENABLE_NASM=OFF and should not require nasm.
    vcpkg_find_acquire_program(NASM)
    set(aom_nasm_compiler "-DCMAKE_ASM_NASM_COMPILER=${NASM}")
endif()

set(aom_target_cpu "")
if(VCPKG_TARGET_IS_UWP OR (VCPKG_TARGET_IS_WINDOWS AND VCPKG_TARGET_ARCHITECTURE MATCHES "^arm"))
    # UWP + aom's assembler files result in weirdness and build failures
    # Also, disable assembly on ARM and ARM64 Windows to fix compilation issues.
    set(aom_target_cpu "-DAOM_TARGET_CPU=generic")
endif()

if(VCPKG_TARGET_ARCHITECTURE STREQUAL "arm" AND VCPKG_TARGET_IS_LINUX)
    set(aom_target_cpu "-DENABLE_NEON=OFF")
endif()

vcpkg_cmake_configure(
    SOURCE_PATH "${SOURCE_PATH}"
    OPTIONS
        ${aom_target_cpu}
        -DENABLE_APPS=OFF
        -DENABLE_DOCS=OFF
        -DENABLE_EXAMPLES=OFF
        -DENABLE_TESTDATA=OFF
        -DENABLE_TESTS=OFF
        -DENABLE_TOOLS=OFF
        -DTHREADS_PREFER_PTHREAD_FLAG=ON
        ${aom_nasm_compiler}
        "-DPERL_EXECUTABLE=${PERL}"
)

vcpkg_cmake_install()
vcpkg_cmake_config_fixup(CONFIG_PATH lib/cmake/AOM)
vcpkg_copy_pdbs()
vcpkg_fixup_pkgconfig()

file(REMOVE_RECURSE
    "${CURRENT_PACKAGES_DIR}/debug/include"
    "${CURRENT_PACKAGES_DIR}/debug/share"
)

vcpkg_install_copyright(FILE_LIST
    "${SOURCE_PATH}/LICENSE"
    "${SOURCE_PATH}/PATENTS"
    "${SOURCE_PATH}/third_party/fastfeat/LICENSE"
    "${SOURCE_PATH}/third_party/vector/LICENSE"
    "${SOURCE_PATH}/third_party/x86inc/LICENSE"
)
