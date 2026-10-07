import { test } from 'node:test';
import assert from 'node:assert/strict';
import { presentError, errorDiagnostic, resultTitle } from '../src/errorPresentation.ts';
import type { ErrorDto, Snapshot, ResultDto } from '../src/types.ts';
const dto = (message: string, code = 'COPY_FAILED', operation = 'copy'): ErrorDto => ({ code, message, operation, retry: '原始建议' });

test('common OS failures produce concise Chinese advice and retain original diagnostics', () => {
  const cases = [
    [5, '没有访问权限'], [32, '文件正在被其他程序占用'], [33, '文件正在被其他程序占用'],
    [39, '磁盘空间不足'], [112, '磁盘空间不足'], [206, '路径或文件名过长'], [2, '找不到所需文件或目录'], [3, '找不到所需文件或目录'],
  ] as const;
  for (const [number, problem] of cases) {
    const error = dto(`C:\\很长的路径\\data.zip: system detail (os error ${number})`);
    const view = presentError(error);
    assert.ok(view.problem.includes(problem)); assert.ok(view.action.length > 0);
    assert.ok(!/os error|COPY_FAILED|C:\\/.test(view.problem + view.action));
    assert.deepEqual(JSON.parse(errorDiagnostic(error, null)).error, error);
    assert.ok(view.action.includes('保留源文件和恢复记录'));
  }
});
test('source and target missing are distinguished only where backend context establishes identity', () => {
  assert.equal(presentError(dto('目标祖先不是普通目录：\\\\?\\C:\\不存在: 系统找不到指定的文件。 (os error 2)', 'PLAN_FAILED', 'preview')).problem, '找不到目标目录。');
  assert.equal(presentError(dto('目标根不是普通目录: C:\\不存在: missing (os error 2)', 'PLAN_FAILED', 'preview')).problem, '找不到目标目录。');
  assert.equal(presentError(dto('missing (os error 3)', 'INVALID_INPUT', 'preview')).problem, '找不到源文件或文件夹。');
  assert.equal(presentError(dto('missing (os error 2)', 'RECOVERY_FAILED', 'resume')).problem, '任务无法继续：找不到所需文件或目录。');
});
test('stale, busy, cancellation request and unknown states never invent completion or a retry', () => {
  for (const code of ['BUSY','STALE_PLAN','STALE_REQUEST','STALE_JOB','STALE_RECOVERY','SHUTTING_DOWN','IPC_FAILED','WORKER_FAILED']) {
    const view = presentError(dto('technical details', code)); assert.ok(!/[A-Z_]/.test(view.problem + view.action));
  }
  for (const message of ['C:\\disk full\\os error 112.txt', 'Source changed during copy.txt', 'something (os error 112) trailing', '程序已取消']) {
    assert.equal(presentError(dto(message, 'UNKNOWN')).problem, '这次操作未能完成。');
  }
  assert.equal(presentError(dto('Source changed during copy')).problem, '源文件已发生变化。');
  assert.equal(presentError(dto('Unknown job metadata entry; preserved', 'RECOVERY_FAILED', 'resume')).problem, '任务暂时无法安全继续。');
  assert.ok(presentError(dto('Unknown job metadata entry; preserved', 'RECOVERY_FAILED')).action.includes('保留源文件和恢复记录'));
});
test('diagnostic includes matching task identifiers, state, paths and raw issues, never unrelated task', () => {
  const error = dto('original <script> & path');
  const task = { job_id: 'opaque-job', phase: 'failed', label: 'C:\\中文路径', error, results: [{ core_job_id: 'core-id', issues: ['完整原始错误'] }] } as unknown as Snapshot;
  assert.deepEqual(JSON.parse(errorDiagnostic(error, task)), { error, task });
  assert.ok(!('task' in JSON.parse(errorDiagnostic(dto('other'), task))));
  assert.ok(!('task' in JSON.parse(errorDiagnostic({ ...error }, task))), 'same text from another task is not associated');
});
test('published and cancellation labels respect actual state and unknown outcomes stay explicit', () => {
  const result = (published: boolean, outcome: string) => ({ published, outcome }) as ResultDto;
  assert.equal(resultTitle(result(false, 'Cancelled')), '已取消，未发布');
  assert.equal(resultTitle(result(false, 'Completed')), '未发布');
  assert.equal(resultTitle(result(true, 'Completed')), '已验证并发布');
  assert.equal(resultTitle(result(false, 'Unknown')), '未发布，结果待核实');
  assert.equal(resultTitle(result(true, 'Unknown')), '已发布，结果待核实');
  assert.equal(resultTitle(result(false, 'constructor')), '未发布，结果待核实');
  assert.equal(presentError(dto('unknown', 'constructor')).problem, '这次操作未能完成。');
});
