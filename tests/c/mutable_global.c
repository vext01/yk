// Run-time:
//   env-var: YKD_LOG_IR=jit-pre-opt
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG=4
//   stderr:
//     yk-jit-event: start-tracing
//     i=4, g=1000
//     yk-jit-event: stop-tracing
//     --- Begin jit-pre-opt ---
//     ..~
//     %{{14}}: ptr = lookup_global @g
//     %{{15}}: i32 = load %{{14}}
//     %{{16}}: i32 = add %{{15}}, 5i32
//     ...
//     --- End jit-pre-opt ---
//     i=3, g=1005
//     yk-jit-event: enter-jit-code
//     i=2, g=1010
//     i=1, g=1015
//     yk-jit-event: deoptimise
//     ...

// Check that mutating a global works.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

int g = 1000;

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();
  int i = 4;
  NOOPT_VAL(i);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    NOOPT_VAL(g);
    fprintf(stderr, "i=%d, g=%d\n", i, g);
    g += 5;
    i--;
  }
  yk_location_drop(loc);
  yk_mt_shutdown(mt);

  return (EXIT_SUCCESS);
}
