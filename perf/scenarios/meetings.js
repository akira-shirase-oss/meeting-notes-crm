// M1's slice of END_GOAL's review-api scenario: reviewers reading meetings and transcripts,
// while recorders push new meetings through the whole pipeline.
//
// reviewers: 64 concurrent readers (a closed model: each waits for its answer) share TOTAL
// requests: 50% meeting lists, 25% one meeting, 25% transcript pages, token-authenticated
// against a meeting the setup took through the pipeline.
// recorders: 4 concurrent members, RECORDINGS times between them: create a meeting, attest
// consent, upload the call (a PUT body), wait for the worker to take it through the pinned
// ffmpeg and the recorded provider to `transcribed`, read the transcript, delete it. Every
// recording must reach `transcribed`: a busy, rejected or stuck one is a failure.
// Approve/reject and their 409s arrive with M3's proposals.
//
// Env: BASE_URL, TOKEN, COOKIE (the member's session, for writes), WORKSPACE, MEETING,
// AUDIO (the call file), MODE (smoke | full); TOTAL and RECORDINGS for plumbing probes.
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

const MODE = __ENV.MODE || 'smoke';
const TOTAL = __ENV.TOTAL ? parseInt(__ENV.TOTAL, 10) : MODE === 'full' ? 1000000 : 100000;
const BASE = `${__ENV.BASE_URL}/v1/workspaces/${__ENV.WORKSPACE}/meetings`;
const LIST = `${BASE}?limit=50`;
const SHOW = `${BASE}/${__ENV.MEETING}`;
const PAGE = `${BASE}/${__ENV.MEETING}/transcript?limit=500`;
const headers = { Authorization: `Bearer ${__ENV.TOKEN}` };
const RECORDINGS = __ENV.RECORDINGS
  ? parseInt(__ENV.RECORDINGS, 10)
  : MODE === 'full' ? 2000 : 200;
const AUDIO = open(__ENV.AUDIO, 'b');
const member = { Cookie: __ENV.COOKIE, 'Content-Type': 'application/json' };
const PIPELINE_DEADLINE_MS = 60000;

const errors = new Rate('errors'); // anything but 200, 409 and 429
const unexpected = new Counter('unexpected_status');
const pipelineFailed = new Rate('pipeline_failed'); // a recording that did not reach transcribed
const pipelineMs = new Trend('pipeline_ms', true); // upload accepted to transcribed
const pipelines = new Counter('pipelines');

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    reviewers: { executor: 'shared-iterations', vus: 64, iterations: TOTAL, maxDuration: '60m' },
    recorders: {
      executor: 'shared-iterations',
      vus: 4,
      iterations: RECORDINGS,
      maxDuration: '60m',
      exec: 'record',
    },
  },
  thresholds: {
    // Written before the first run, from END_GOAL's review-api pass line.
    errors: ['rate<0.001'],
    'http_req_duration{kind:read}': ['p(99)<100'],
    unexpected_status: ['count<1000'],
    pipeline_failed: ['rate==0'],
    pipelines: [`count>=${RECORDINGS}`],
    pipeline_ms: ['p(99)<30000'],
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

function write(method, url, body, expect) {
  const res = http.request(method, url, body, {
    headers: member,
    responseType: 'text',
    tags: { kind: 'write' },
  });
  if (res.status !== expect) {
    unexpected.add(1);
    return null;
  }
  return res;
}

export function record() {
  const created = write('POST', BASE, JSON.stringify({ title: 'Load recording' }), 201);
  if (!created) return pipelineFailed.add(true);
  const url = `${BASE}/${created.json('id')}`;
  const consent = JSON.stringify({ statement: 'all_participants_consented', participants: 2 });
  if (!write('POST', `${url}/consent`, consent, 201)) return pipelineFailed.add(true);
  const upload = http.put(`${url}/media`, AUDIO, {
    headers: { Cookie: __ENV.COOKIE, 'Content-Type': 'application/octet-stream' },
    responseType: 'none',
    tags: { kind: 'upload' },
  });
  if (!check(upload, { 'upload 202': (r) => r.status === 202 })) {
    unexpected.add(1);
    return pipelineFailed.add(true);
  }
  const accepted = Date.now();
  let status = '';
  while (Date.now() - accepted < PIPELINE_DEADLINE_MS) {
    const shown = http.get(url, { headers: member, responseType: 'text', tags: { kind: 'poll' } });
    status = shown.status === 200 ? shown.json('status') : '';
    if (status !== 'receiving' && status !== 'transcoding' && status !== 'transcribing') break;
    sleep(0.1);
  }
  const done = status === 'transcribed';
  if (done) pipelineMs.add(Date.now() - accepted);
  const page = done ? http.get(`${url}/transcript`, { headers: member, responseType: 'text' }) : null;
  const read = page !== null && page.status === 200 && page.json('segments').length === 4;
  pipelineFailed.add(!read);
  if (read) pipelines.add(1);
  write('DELETE', url, null, 204);
}
