/* Offsets the Python frame walk hardcodes, checked against the headers of
 * the interpreter that is actually installed.
 *
 * Build with the include directory of one CPython:
 *
 *   gcc -std=c11 -Wall -Werror -DPy_BUILD_CORE \
 *       -I"$INC" -I"$INC/internal" -o cpython-offsets cpython_offsets.c
 *
 * The walk accepts only the default GIL layouts of 3.11, 3.12, and 3.13.
 * A later minor, a free-threaded build, and a header that moved one of
 * these fields fail this program. CI compiles it three times, once per
 * installed version. `cargo test` does not, so the MSRV job does not need
 * a C compiler or a Python development package.
 *
 * The numbers are LP64. On Windows x64, `unsigned long` is 4 bytes, so
 * `native_thread_id` is 4 bytes earlier (3.11: 156, 3.12: 140). The other
 * fields this walk uses are pointers or `int` and keep these offsets.
 */
#define Py_BUILD_CORE 1
#include <Python.h>
#include <internal/pycore_frame.h>
#include <internal/pycore_interp.h>
#include <internal/pycore_runtime.h>
#if PY_VERSION_HEX >= 0x030E0000
#include <internal/pycore_debug_offsets.h>
#include <internal/pycore_stackref.h>
#endif

#include <stdio.h>
#include <stddef.h>

static int failures = 0;

static void expect(const char *label, size_t got, size_t want) {
    if (got != want) {
        fprintf(stderr, "%s is %zu, the walk uses %zu\n", label, got, want);
        failures++;
    }
}

int main(void) {
#if PY_VERSION_HEX >= 0x030B0000 && PY_VERSION_HEX < 0x030C0000
    expect("interpreters.head", offsetof(_PyRuntimeState, interpreters.head), 40);
    expect("interp.next", offsetof(PyInterpreterState, next), 0);
    expect("threads.head", offsetof(PyInterpreterState, threads.head), 16);
    expect("tstate.next", offsetof(PyThreadState, next), 8);
    expect("tstate.cframe", offsetof(PyThreadState, cframe), 56);
    expect("tstate.native_thread_id", offsetof(PyThreadState, native_thread_id), 160);
    expect("cframe.current_frame", offsetof(_PyCFrame, current_frame), 8);
    expect("frame.f_code", offsetof(_PyInterpreterFrame, f_code), 32);
    expect("frame.previous", offsetof(_PyInterpreterFrame, previous), 48);
    expect("frame.prev_instr", offsetof(_PyInterpreterFrame, prev_instr), 56);
    expect("code.filename", offsetof(PyCodeObject, co_filename), 112);
    expect("code.qualname", offsetof(PyCodeObject, co_qualname), 128);
    expect("code.linetable", offsetof(PyCodeObject, co_linetable), 136);
    expect("code.firstlineno", offsetof(PyCodeObject, co_firstlineno), 72);
    expect("code.adaptive", offsetof(PyCodeObject, co_code_adaptive), 184);
    /* `use_tracing` makes current_frame 8, and `wstr` makes compact ASCII
     * data start at 48. Non-ASCII compact data follows wstr_length, at 72. */
    expect("ascii", sizeof(PyASCIIObject), 48);
    expect("compact", sizeof(PyCompactUnicodeObject), 72);
    expect("unicode.state", offsetof(PyASCIIObject, state), 32);
    expect("unicode.length", offsetof(PyASCIIObject, length), 16);
    expect("bytes.ob_size", offsetof(PyVarObject, ob_size), 16);
    expect("bytes.ob_sval", offsetof(PyBytesObject, ob_sval), 32);
#elif PY_VERSION_HEX >= 0x030C0000 && PY_VERSION_HEX < 0x030D0000
    expect("interpreters.head", offsetof(_PyRuntimeState, interpreters.head), 40);
    expect("interp.next", offsetof(PyInterpreterState, next), 0);
    expect("threads.head", offsetof(PyInterpreterState, threads.head), 72);
    expect("tstate.next", offsetof(PyThreadState, next), 8);
    expect("tstate.cframe", offsetof(PyThreadState, cframe), 56);
    expect("tstate.native_thread_id", offsetof(PyThreadState, native_thread_id), 144);
    expect("cframe.current_frame", offsetof(_PyCFrame, current_frame), 0);
    expect("frame.f_code", offsetof(_PyInterpreterFrame, f_code), 0);
    expect("frame.previous", offsetof(_PyInterpreterFrame, previous), 8);
    expect("frame.prev_instr", offsetof(_PyInterpreterFrame, prev_instr), 56);
    expect("frame.owner", offsetof(_PyInterpreterFrame, owner), 70);
    expect("code.filename", offsetof(PyCodeObject, co_filename), 112);
    expect("code.qualname", offsetof(PyCodeObject, co_qualname), 128);
    expect("code.linetable", offsetof(PyCodeObject, co_linetable), 136);
    expect("code.firstlineno", offsetof(PyCodeObject, co_firstlineno), 68);
    expect("code.adaptive", offsetof(PyCodeObject, co_code_adaptive), 192);
    expect("ascii", sizeof(PyASCIIObject), 40);
    expect("compact", sizeof(PyCompactUnicodeObject), 56);
    expect("unicode.state", offsetof(PyASCIIObject, state), 32);
    expect("unicode.length", offsetof(PyASCIIObject, length), 16);
    expect("bytes.ob_size", offsetof(PyVarObject, ob_size), 16);
    expect("bytes.ob_sval", offsetof(PyBytesObject, ob_sval), 32);
#elif PY_VERSION_HEX >= 0x030D0000 && PY_VERSION_HEX < 0x030E0000
    /* 3.13's walk reads `_Py_DebugOffsets` from the binary. These are the
     * slot positions in that table, not the struct offsets they point at. */
    expect("free_threaded", offsetof(_Py_DebugOffsets, free_threaded), 16);
    expect("runtime.size", offsetof(_Py_DebugOffsets, runtime_state.size), 24);
    expect("runtime.head", offsetof(_Py_DebugOffsets, runtime_state.interpreters_head), 40);
    expect("interp.size", offsetof(_Py_DebugOffsets, interpreter_state.size), 48);
    expect("interp.next", offsetof(_Py_DebugOffsets, interpreter_state.next), 64);
    expect("interp.threads", offsetof(_Py_DebugOffsets, interpreter_state.threads_head), 72);
    expect("thread.size", offsetof(_Py_DebugOffsets, thread_state.size), 152);
    expect("thread.next", offsetof(_Py_DebugOffsets, thread_state.next), 168);
    expect("thread.frame", offsetof(_Py_DebugOffsets, thread_state.current_frame), 184);
    expect("thread.native", offsetof(_Py_DebugOffsets, thread_state.native_thread_id), 200);
    expect("frame.size", offsetof(_Py_DebugOffsets, interpreter_frame.size), 224);
    expect("frame.previous", offsetof(_Py_DebugOffsets, interpreter_frame.previous), 232);
    expect("frame.executable", offsetof(_Py_DebugOffsets, interpreter_frame.executable), 240);
    expect("frame.instr", offsetof(_Py_DebugOffsets, interpreter_frame.instr_ptr), 248);
    expect("frame.owner", offsetof(_Py_DebugOffsets, interpreter_frame.owner), 264);
    expect("code.size", offsetof(_Py_DebugOffsets, code_object.size), 272);
    expect("code.filename", offsetof(_Py_DebugOffsets, code_object.filename), 280);
    expect("code.qualname", offsetof(_Py_DebugOffsets, code_object.qualname), 296);
    expect("code.linetable", offsetof(_Py_DebugOffsets, code_object.linetable), 304);
    expect("code.firstlineno", offsetof(_Py_DebugOffsets, code_object.firstlineno), 312);
    expect("code.adaptive", offsetof(_Py_DebugOffsets, code_object.co_code_adaptive), 344);
    expect("bytes.size", offsetof(_Py_DebugOffsets, bytes_object.size), 512);
    expect("bytes.ob_size", offsetof(_Py_DebugOffsets, bytes_object.ob_size), 520);
    expect("bytes.ob_sval", offsetof(_Py_DebugOffsets, bytes_object.ob_sval), 528);
    expect("unicode.size", offsetof(_Py_DebugOffsets, unicode_object.size), 536);
    expect("unicode.state", offsetof(_Py_DebugOffsets, unicode_object.state), 544);
    expect("unicode.length", offsetof(_Py_DebugOffsets, unicode_object.length), 552);
    expect("unicode.ascii", offsetof(_Py_DebugOffsets, unicode_object.asciiobject_size), 560);
    expect("ascii", sizeof(PyASCIIObject), 40);
    expect("compact", sizeof(PyCompactUnicodeObject), 56);
    if (_Py_Debug_Free_Threaded != 0) {
        fprintf(stderr, "this header is the free-threaded build, which is not walked\n");
        failures++;
    }
#elif PY_VERSION_HEX >= 0x030E0000 && PY_VERSION_HEX < 0x030F0000
    /* 3.14 adds fields to `interpreter_state`, `interpreter_frame`,
     * `code_object`, and `set_object`, shifting `_Py_DebugOffsets` slots, and
     * stores a tagged `_PyStackRef` (`Py_TAG_BITS = 3`) in `f_executable`. */
    expect("free_threaded", offsetof(_Py_DebugOffsets, free_threaded), 16);
    expect("runtime.size", offsetof(_Py_DebugOffsets, runtime_state.size), 24);
    expect("runtime.head", offsetof(_Py_DebugOffsets, runtime_state.interpreters_head), 40);
    expect("interp.size", offsetof(_Py_DebugOffsets, interpreter_state.size), 48);
    expect("interp.next", offsetof(_Py_DebugOffsets, interpreter_state.next), 64);
    expect("interp.threads", offsetof(_Py_DebugOffsets, interpreter_state.threads_head), 72);
    expect("thread.size", offsetof(_Py_DebugOffsets, thread_state.size), 176);
    expect("thread.next", offsetof(_Py_DebugOffsets, thread_state.next), 192);
    expect("thread.frame", offsetof(_Py_DebugOffsets, thread_state.current_frame), 208);
    expect("thread.native", offsetof(_Py_DebugOffsets, thread_state.native_thread_id), 224);
    expect("frame.size", offsetof(_Py_DebugOffsets, interpreter_frame.size), 248);
    expect("frame.previous", offsetof(_Py_DebugOffsets, interpreter_frame.previous), 256);
    expect("frame.executable", offsetof(_Py_DebugOffsets, interpreter_frame.executable), 264);
    expect("frame.instr", offsetof(_Py_DebugOffsets, interpreter_frame.instr_ptr), 272);
    expect("frame.owner", offsetof(_Py_DebugOffsets, interpreter_frame.owner), 288);
    expect("code.size", offsetof(_Py_DebugOffsets, code_object.size), 312);
    expect("code.filename", offsetof(_Py_DebugOffsets, code_object.filename), 320);
    expect("code.qualname", offsetof(_Py_DebugOffsets, code_object.qualname), 336);
    expect("code.linetable", offsetof(_Py_DebugOffsets, code_object.linetable), 344);
    expect("code.firstlineno", offsetof(_Py_DebugOffsets, code_object.firstlineno), 352);
    expect("code.adaptive", offsetof(_Py_DebugOffsets, code_object.co_code_adaptive), 384);
    expect("bytes.size", offsetof(_Py_DebugOffsets, bytes_object.size), 592);
    expect("bytes.ob_size", offsetof(_Py_DebugOffsets, bytes_object.ob_size), 600);
    expect("bytes.ob_sval", offsetof(_Py_DebugOffsets, bytes_object.ob_sval), 608);
    expect("unicode.size", offsetof(_Py_DebugOffsets, unicode_object.size), 616);
    expect("unicode.state", offsetof(_Py_DebugOffsets, unicode_object.state), 624);
    expect("unicode.length", offsetof(_Py_DebugOffsets, unicode_object.length), 632);
    expect("unicode.ascii", offsetof(_Py_DebugOffsets, unicode_object.asciiobject_size), 640);
    expect("ascii", sizeof(PyASCIIObject), 40);
    expect("compact", sizeof(PyCompactUnicodeObject), 56);
    expect("stackref.tag_bits", Py_TAG_BITS, 3);
    expect("stackref.tag_refcnt", Py_TAG_REFCNT, 1);
    if (_Py_Debug_Free_Threaded != 0) {
        fprintf(stderr, "this header is the free-threaded build, which is not walked\n");
        failures++;
    }
#else
    fprintf(stderr, "CPython %x is not a walked layout\n", PY_VERSION_HEX);
    return 1;
#endif
    if (failures != 0) {
        fprintf(stderr, "%d offset mismatch(es) in CPython %x\n", failures, PY_VERSION_HEX);
        return 1;
    }
    printf("cpython offsets match %x\n", PY_VERSION_HEX);
    return 0;
}
