// L1 read-hot: concurrent-connection ramp. Each VU stage level = N
// concurrent connections against one arm; spread mode pins each VU to its
// own session (the "concurrent sessions" capacity number), hot mode pins
// every VU to the 32k-msg deep session (contention on the heaviest row).
import { doIteration, buildThresholds, rampStages } from './mix.js';

export const options = {
  scenarios: {
    readhot: {
      executor: 'ramping-vus',
      stages: rampStages(),
      gracefulRampDown: '10s',
      startTime: '0s',
    },
  },
  thresholds: buildThresholds(),
  summaryTrendStats: ['avg', 'p(50)', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
  doIteration();
}
