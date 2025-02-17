// Compiler:
//   env-var: YKB_EXTRA_CC_FLAGS=-O1
// Run-time:
//   env-var: YKD_LOG_IR=aot,jit-pre-opt
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG=4
//   stderr:
//     yk-jit-event: start-tracing
//     i=4, y=7
//     yk-jit-event: stop-tracing
//     --- Begin aot ---
//     ...
//     %{{9_4}}: ptr = ptr_add @line, 4 + (%{{9_3}} * 8)
//     ...
//     --- End aot ---
//     --- Begin jit-pre-opt ---
//     ...
//     %{{14}}: ptr = ptr_add %{{_}}, 4
//     %{{_}}: ptr = dyn_ptr_add %{{14}}, %{{_}}, 8
//     ...
//     --- End jit-pre-opt ---
//     i=3, y=6
//     yk-jit-event: enter-jit-code
//     i=2, y=5
//     i=1, y=4
//     yk-jit-event: deoptimise

// Check dynamic ptradd instructions work.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

struct point {
  uint32_t x;
  uint32_t y;
};

struct point line[] = {
    {3, 3}, {4, 4}, {5, 5}, {6, 6}, {7, 7},
};

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();

  int i = 4;
  NOOPT_VAL(loc);
  NOOPT_VAL(i);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    fprintf(stderr, "i=%d, y=%d\n", i, line[i].y);
    i--;
  }
  yk_location_drop(loc);
  yk_mt_shutdown(mt);
  return (EXIT_SUCCESS);
}
