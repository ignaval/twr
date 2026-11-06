export const ABI_LIMITS: Readonly<Record<string, string>>;
export function elfRequirements(data: Buffer): string[];
export function incompatibleRequirements(requirements: string[]): string[];
export interface LinuxAbiReport {
  baseline: string;
  limits: typeof ABI_LIMITS;
  binaries: {
    path: string;
    requirements: string[];
    incompatible: string[];
  }[];
}
export function checkLinuxBundle(directory: string): LinuxAbiReport;
