import type { Snapshot } from './types.ts';
export function bumpRevision(value: string): string { return (BigInt(value) + 1n).toString(); }
export function formatBytes(value: string): string {
  const bytes = BigInt(value); const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'];
  let divisor = 1n; let unit = 0;
  while (bytes >= divisor * 1024n && unit < units.length - 1) { divisor *= 1024n; unit++; }
  if (!unit) return `${bytes} B`;
  const scaled = bytes * 100n / divisor;
  return `${scaled / 100n}.${(scaled % 100n).toString().padStart(2, '0')} ${units[unit]}`;
}
export function acceptSnapshot(current: Snapshot | null, next: Snapshot, expectedJob: string | null): boolean {
  if (expectedJob && next.job_id !== expectedJob) return false;
  return !current || current.job_id !== next.job_id || BigInt(next.sequence) >= BigInt(current.sequence);
}
export function progressPercent(snapshot: Snapshot): number | null {
  const total = BigInt(snapshot.total_bytes); if (!total) return null;
  const raw = BigInt(snapshot.current_bytes) * 10000n / total;
  return Number(raw > 10000n ? 10000n : raw) / 100;
}
export const phases: Record<string, string> = {recovering:'正在检查并继续恢复',planning:'正在读取并生成预览',preview_ready:'预览就绪',copying:'正在复制与校验',completed:'已验证并发布',cancelled:'已取消',cancel_requested_end:'取消请求后操作已结束，请查看详情',failed:'未完成',history:'读取任务记录',history_loaded:'记录已载入',verifying:'正在校验',verification_finished:'校验结束'};
