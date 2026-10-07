import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formatBytes, acceptSnapshot, bumpRevision, progressPercent } from '../src/model.ts';
import type { Snapshot } from '../src/types.ts';
const snap = (job_id: string, sequence: string): Snapshot => ({ job_id, sequence, revision: '1', phase: 'copying', label: '', current_bytes: '0', total_bytes: '0', terminal: false, cancel_requested: false, results: [], error: null });
test('bytes preserve integer precision past Number.MAX_SAFE_INTEGER', () => {
    assert.equal(formatBytes('9007199254740993'), '8.00 PiB');
    assert.equal(bumpRevision('9007199254740993'), '9007199254740994');
});
test('late snapshot cannot change another job or roll back sequence', () => {
    assert.equal(acceptSnapshot(snap('b', '2'), snap('a', '99'), 'b'), false);
    assert.equal(acceptSnapshot(snap('b', '2'), snap('b', '1'), 'b'), false);
    assert.equal(acceptSnapshot(snap('b', '2'), snap('b', '3'), 'b'), true);
});
test('terminal publication stays distinct from per-file progress', () => {
    const s = snap('a', '1'); s.current_bytes = '10'; s.total_bytes = '10';
    assert.equal(progressPercent(s), 100); assert.equal(s.terminal, false);
    s.total_bytes = '0'; assert.equal(progressPercent(s), null);
});
