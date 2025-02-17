// Run-time:
//   env-var: YKD_LOG_IR=aot,jit-pre-opt,jit-post-opt
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG=4
//   stderr:
//     ...
//     --- Begin aot ---
//     ...
//     call call_me()...
//     ...
//     --- End aot ---
//     ...
//     --- Begin jit-pre-opt ---
//     ...
//     call @call_me()...
//     ...
//     --- End jit-pre-opt ---
//     ...
//     Can't JIT this!
//     Or this!
//     yk-jit-event: enter-jit-code
//     Can't JIT this!
//     Or this!
//     Can't JIT this!
//     Or this!
//     Can't JIT this!
//     Or this!
//     yk-jit-event: deoptimise
//     ...

// Check that the `yk_outline` annotation works when a `yk_outline` annotated
// function calls another function.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

void call_me2(void) { fprintf(stderr, "Or this!\n"); }

__attribute__((yk_outline)) void call_me(void) {
  fprintf(stderr, "Can't JIT this!\n");
  call_me2();
}

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();

  int i = 5;
  NOOPT_VAL(loc);
  NOOPT_VAL(i);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    call_me(); // This call must not be inlined.
    i--;
  }

  yk_location_drop(loc);
  yk_mt_shutdown(mt);
  return (EXIT_SUCCESS);
}
