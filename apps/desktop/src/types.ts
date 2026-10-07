export interface SourceInput { path: string; kind: 'file' | 'directory'; recursive: boolean; suffixes: string; include_extensionless: boolean }
export interface PreviewRequest { revision: string; target: string; sources: SourceInput[] }
export interface ErrorDto { code: string; message: string; operation: string; retry: string }
export interface PlanDto { plan_id: string; revision: string; target: string; files: string; directories: string; bytes: string; entry_count: string; destinations: string[]; issues: string[] }
export interface EntryDto { source: string; relative_path: string; kind: string; bytes: string }
export interface ResultDto { core_job_id: string; outcome: string; published: boolean; copied: string; verified: string; skipped_verified: string; failed: string; bytes: string; issues: string[] }
export interface Snapshot { job_id: string; sequence: string; revision: string; phase: string; label: string; current_bytes: string; total_bytes: string; terminal: boolean; cancel_requested: boolean; results: ResultDto[]; error: ErrorDto | null }
export interface HistoryDto { core_job_id: string; source: string; destination: string; bytes: string; completed: boolean; state: string; recursive: boolean; recovery_id: string | null }
export interface VerificationDto { checked: string; bad: string; size_only: string; extras: string; cancelled: boolean; issues: string[] }
