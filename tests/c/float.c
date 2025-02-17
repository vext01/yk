// Compiler:
//   env-var: YKB_EXTRA_CC_FLAGS=-O1
// Run-time:
//   env-var: YKD_LOG_IR=aot,jit-pre-opt
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG=4
//   stderr:
//     yk-jit-event: start-tracing
//     4 -> 4.000000
//     yk-jit-event: stop-tracing
//     --- Begin aot ---
//     ...
//     func main(%arg0: i32, %arg1: ptr) -> i32 {
//     ...
//     %{{9_3}}: float = si_to_fp %{{9_2}}, float
//     %{{9_4}}: double = fp_ext %{{9_3}}, double
//     ...
//     %{{9_7}}: i32 = call fprintf(%{{_}}, @{{_}}, %{{9_2}}, %{{9_4}})
//     ...
//     --- End aot ---
//     --- Begin jit-pre-opt ---
//     ...
//     %{{12}}: float = si_to_fp %{{11}}
//     %{{13}}: double = fp_ext %{{12}}
//     ...
//     %{{_}}: i32 = call @fprintf(%{{_}}, %{{_}}, %{{11}}, %{{13}})
//     ...
//     --- End jit-pre-opt ---
//     3 -> 3.000000
//     yk-jit-event: enter-jit-code
//     2 -> 2.000000
//     1 -> 1.000000
//     yk-jit-event: deoptimise

// Check basic 32-bit float support.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();

  int i = 4;
  NOOPT_VAL(loc);
  NOOPT_VAL(i);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    fprintf(stderr, "%d -> %f\n", i, (float)i);
    i--;
  }
  yk_location_drop(loc);
  yk_mt_shutdown(mt);
  return (EXIT_SUCCESS);
}
