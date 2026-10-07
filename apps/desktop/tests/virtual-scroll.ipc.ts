// Only the isolated component regression uses this replacement. Production IPC is unchanged.
import type { EntryDto } from '../src/types';
type PageRequest = { planId: string; offset: number; limit: number };
export function invoke<T>(command: string, args: PageRequest): Promise<T> {
  if (command !== 'plan_entries') throw new Error('Unexpected fixture command');
  const fixture = (window as unknown as { __planEntriesFixture: (args: PageRequest) => Promise<EntryDto[]> }).__planEntriesFixture;
  return fixture(args) as Promise<T>;
}
