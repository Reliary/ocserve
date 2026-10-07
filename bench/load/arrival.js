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
      // 500 preallocated (was 200): with rate=4480 the first seconds of
      // VU allocation produced ~6k dropped iterations (~2.1% of offered)
      // before steady state — client-side allocation lag, not server
      // capacity (closed runs do 9.2k req/s). maxVUs kept >= rate x worst
      // plausible mean (4480 x 0.022s = 99 -> huge headroom at 1500).
      preAllocatedVUs: 500,
      maxVUs: 1500,
    },
  },
  thresholds: buildThresholds(),
  summaryTrendStats: ['avg', 'p(50)', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
  doIteration();
}
