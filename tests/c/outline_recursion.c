// Run-time:
//   env-var: YKD_LOG_IR=aot,jit-pre-opt
//   env-var: YKD_SERIALISE_COMPILATION=1
//   env-var: YKD_LOG=4
//   stderr:
//     yk-jit-event: start-tracing
//     0
//     1
//     2
//     3
//     4
//     yk-jit-event: stop-tracing
//     --- Begin aot ---
//     ...
//     func main(%arg0: i32, %arg1: ptr) -> i32 {
//     ...
//     --- End aot ---
//     --- Begin jit-pre-opt ---
//     ...
//     call @foo(%{{4}})
//     ...
//     --- End jit-pre-opt ---
//     0
//     1
//     2
//     3
//     yk-jit-event: enter-jit-code
//     0
//     1
//     2
//     0
//     1
//     yk-jit-event: deoptimise
//     0
//     exit

// Test outlining of recursive calls.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <yk.h>
#include <yk_testing.h>

__attribute__((noinline)) void foo(int i) {
  if (i > 0) {
    foo(i - 1);
    fprintf(stderr, "%d\n", i);
    return;
  }
  fprintf(stderr, "%d\n", i);
  return;
}

int main(int argc, char **argv) {
  YkMT *mt = yk_mt_new(NULL);
  yk_mt_hot_threshold_set(mt, 0);
  YkLocation loc = yk_location_new();

  int i = 4;
  NOOPT_VAL(loc);
  NOOPT_VAL(i);
  while (i > 0) {
    yk_mt_control_point(mt, &loc);
    foo(i);
    i--;
  }
  fprintf(stderr, "%d\n", i);
  fprintf(stderr, "exit\n");
  yk_location_drop(loc);
  yk_mt_shutdown(mt);
  return (EXIT_SUCCESS);
}
