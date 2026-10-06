// Threshold-wiring self-test (no server needed — the anti-theater control
// for the whole suite): the harness runs this twice and demands the exit
// codes below. `iterations` is a metric that always exists with exactly
// one sample per run, so both directions are deterministic.
//   clean run            → threshold count>0 passes      → rc 0
//   LOAD_THRESH_FAIL=1   → threshold count>99 breaches   → rc != 0
//   --no-thresholds      → breach arg ignored            → rc 0
export const options = {
  thresholds: {
    iterations: [__ENV.LOAD_THRESH_FAIL ? 'count>99' : 'count>0'],
  },
};

export default function () {}
