// M1's slice of END_GOAL's review-api scenario: reviewers reading meetings and transcripts.
// 64 concurrent reviewers (a closed model: each waits for its answer) share the requests:
// 50% meeting lists, 25% one meeting, 25% transcript pages, all token-authenticated against
// a meeting the setup took through the whole pipeline (upload, the pinned ffmpeg, the
// recorded provider). Approve/reject and their 409s arrive with M3's proposals.
//
// Env: BASE_URL, TOKEN, WORKSPACE, MEETING, MODE (smoke | full); TOTAL for plumbing probes.
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate } from 'k6/metrics';

const MODE = __ENV.MODE || 'smoke';
const TOTAL = __ENV.TOTAL ? parseInt(__ENV.TOTAL, 10) : MODE === 'full' ? 1000000 : 100000;
const BASE = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.WORKSPACE}/meetings`;
const LIST = `${BASE}?limit=50`;
const SHOW = `${BASE}/${__ENV.MEETING}`;
const PAGE = `${BASE}/${__ENV.MEETING}/transcript?limit=500`;
const headers = { Authorization: `Bearer ${__ENV.TOKEN}` };

const errors = new Rate('errors'); // anything but 200, 409 and 429
const unexpected = new Counter('unexpected_status');

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    reviewers: { executor: 'shared-iterations', vus: 64, iterations: TOTAL, maxDuration: '60m' },
  },
  thresholds: {
    // Written before the first run, from END_GOAL's review-api pass line.
    errors: ['rate<0.001'],
    'http_req_duration{kind:read}': ['p(99)<100'],
    unexpected_status: ['count<1000'],
  },
};

export default function () {
  const r = Math.random();
  const url = r < 0.5 ? LIST : r < 0.75 ? SHOW : PAGE;
  const res = http.get(url, { headers, tags: { kind: 'read' } });
  const ok = res.status === 200;
  errors.add(!(ok || res.status === 409 || res.status === 429));
  if (!check(res, { 'status 200': () => ok })) unexpected.add(1);
}
