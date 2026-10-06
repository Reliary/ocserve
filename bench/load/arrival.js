// L1 arrival: open-model fixed offered rate — achieved vs offered RPS is
// the queueing/saturation signal (ramp finds the knee, this quantifies it
// at a declared rate). Same route mix as read-hot.
import { doIteration, buildThresholds } from './mix.js';

const RATE = Number(__ENV.LOAD_RPS || 200);
const DUR = __ENV.LOAD_ARRIVAL_DUR || '60s';

export const options = {
  scenarios: {
    arrival: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      duration: DUR,
      timeUnit: '1s',
      preAllocatedVUs: 200,
      maxVUs: 1000,
    },
  },
  thresholds: buildThresholds(),
  summaryTrendStats: ['avg', 'p(50)', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
  doIteration();
}
