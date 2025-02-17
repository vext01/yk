// Run-time:
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG_IR=jit-pre-opt
//   env-var: YKD_LOG=4
//   stderr:
//     yk-jit-event: start-tracing
//     z=3
//     yk-jit-event: stop-tracing
//     --- Begin jit-pre-opt ---
//     ...
//     %{{17}}: i64 = icall %{{8}}(%{{16}})
//     ...
//     --- End jit-pre-opt ---
//     z=3
//     yk-jit-event: enter-jit-code
//     z=3
//     yk-jit-event: deoptimise

// Test indirect calls where we don't have IR for the callee.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

int bar(size_t (*func)(const char *)) {
  int a = func("abc");
  return a;
}

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();

  int z = 0, i = 3;
  size_t (*f)(const char *) = strlen;
  NOOPT_VAL(i);
  NOOPT_VAL(z);
  NOOPT_VAL(f);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    z = bar(f);
    fprintf(stderr, "z=%d\n", z);
    i--;
  }
  NOOPT_VAL(z);
  assert(z == 3);

  yk_location_drop(loc);
  yk_mt_shutdown(mt);
  return (EXIT_SUCCESS);
}
