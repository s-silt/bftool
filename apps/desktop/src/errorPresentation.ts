import type { ErrorDto, ResultDto, Snapshot } from './types.ts';

export interface ErrorPresentation { problem: string; action: string }
const keep = '保留源文件和恢复记录，查看详情后再处理。';

// Presentation only. Never use these messages to determine job state or retry.
export function presentError(error: ErrorDto): ErrorPresentation {
  const known: Record<string, ErrorPresentation> = {
    BUSY: { problem: '还有任务正在运行。', action: '请等待任务结束，或先安全取消。' },
    STALE_PLAN: { problem: '预览已失效。', action: '请重新生成预览，再开始复制。' },
    STALE_REQUEST: { problem: '设置已变更，原请求已失效。', action: '请按当前设置重新预览或加载记录。' },
    STALE_RECOVERY: { problem: '这条恢复记录已失效。', action: '请重新加载原目标目录的任务记录。' },
    STALE_JOB: { problem: '当前任务已结束或已变更。', action: '请查看最新任务状态，再决定下一步。' },
    SHUTTING_DOWN: { problem: '程序正在安全关闭。', action: '请等待关闭完成，需要时再重新打开。' },
    IPC_FAILED: { problem: '无法获取任务信息。', action: '请查看详情；确认任务状态前不要重复开始复制。' },
    WORKER_FAILED: { problem: '后台任务未能正常完成。', action: keep },
  };
  if (Object.hasOwn(known, error.code)) return known[error.code]!;
  const reason = error.message.split(': ')[0];
  const changed = new Set(['Source changed before copy', 'Source changed during copy',
    'Source changed while hashing', 'Source identity changed while hashing',
    'Source tree changed since preview', 'Selected source file identity changed while planning']);
  if (changed.has(reason!)) return { problem: '源文件已发生变化。', action: '请先保留源文件和恢复记录，确认文件状态后重新预览。' };
  if (reason === 'Recovery source/target identity or contents changed') return {
    problem: '源文件或目标内容已变化，无法安全恢复。', action: keep };
  if (reason === 'Planned destination is now occupied; replan required') return {
    problem: '预览中的目标位置已被占用。', action: '请保留源文件和恢复记录，重新预览以生成新目标名称。' };
  // Only a terminal Rust OS error marker is interpreted. Words/numbers in paths
  // must not masquerade as a cause. Values follow the installed Windows SDK.
  const os = error.message.match(/\(os error (\d+)\)\s*$/)?.[1];
  let view: ErrorPresentation | undefined;
  if (os === '5') view = { problem: '没有访问权限。', action: '请检查文件及目标目录权限，或选择有权限的目标目录。' };
  if (os === '32' || os === '33') view = { problem: '文件正在被其他程序占用。', action: '请关闭占用文件的程序，再手动重试。' };
  if (os === '39' || os === '112') view = { problem: '磁盘空间不足。', action: '请检查目标磁盘空间，或选择空间足够的目标目录。' };
  if (os === '206') view = { problem: '路径或文件名过长。', action: '请尝试较短的目标目录；需要修改源名称时先保留恢复记录。' };
  if (os === '2' || os === '3') {
    const target = error.code === 'PLAN_FAILED' && (error.message.startsWith('目标根不是普通目录:')
      || error.message.startsWith('目标祖先不是普通目录：'));
    const source = error.code === 'INVALID_INPUT' && error.operation === 'preview';
    view = target
      ? { problem: '找不到目标目录。', action: '请选择已存在的普通文件夹，再生成预览。' }
      : source
        ? { problem: '找不到源文件或文件夹。', action: '请确认源文件仍在原位置，再重新选择并预览。' }
        : { problem: '找不到所需文件或目录。', action: '请检查源文件及目标目录是否仍在原位置。' };
  }
  if (error.code === 'RECOVERY_FAILED') return view
    ? { problem: `任务无法继续：${view.problem}`, action: `${view.action} ${keep}` }
    : { problem: '任务暂时无法安全继续。', action: keep };
  if (view) return { ...view, action: error.code === 'COPY_FAILED' ? `${view.action} 保留源文件和恢复记录。` : view.action };
  if (error.code === 'INVALID_INPUT') return error.operation === 'suffixes'
    ? { problem: '后缀筛选格式不正确。', action: '请用英文逗号分隔后缀，例如 zip, tar。' }
    : { problem: '当前设置无法使用。', action: '请检查源路径、目标目录及筛选设置，再重新预览。' };
  const fallback: Record<string, ErrorPresentation> = {
    PLAN_FAILED: { problem: '无法生成预览。', action: '请检查源文件与目标目录，展开详情查看原因。' },
    COPY_FAILED: { problem: '复制未能完成。', action: keep },
    HISTORY_FAILED: { problem: '无法读取任务记录。', action: '请确认选择了原目标目录，保留恢复记录并查看详情。' },
    VERIFY_FAILED: { problem: '校验未能完成。', action: '请保留源文件，查看详情后再检查备份。' },
  };
  return Object.hasOwn(fallback, error.code) ? fallback[error.code]! : { problem: '这次操作未能完成。', action: keep };
}

export function errorDiagnostic(error: ErrorDto, snapshot: Snapshot | null): string {
  // A concurrently displayed snapshot must not be attributed to another error.
  const associated = snapshot?.error === error;
  return JSON.stringify({ error, ...(associated ? { task: snapshot } : {}) }, null, 2);
}

export function resultTitle(result: ResultDto): string {
  if (result.published) return result.outcome === 'Completed' ? '已验证并发布' : '已发布，结果待核实';
  const titles: Record<string, string> = { Cancelled: '已取消，未发布', Failed: '失败，未发布', Completed: '未发布' };
  return Object.hasOwn(titles, result.outcome) ? titles[result.outcome]! : '未发布，结果待核实';
}
