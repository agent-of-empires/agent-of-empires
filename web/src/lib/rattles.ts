/** Spinner frames from rattles (https://github.com/vyfor/rattles). */
export const RATTLES = {
  dots: {
    frames: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    interval: 220,
  },
  orbit: { frames: ["⠃", "⠉", "⠘", "⠰", "⢠", "⣀", "⡄", "⠆"], interval: 400 },
  breathe: {
    frames: ["⠀", "⠂", "⠌", "⡑", "⢕", "⢝", "⣫", "⣟", "⣿", "⣟", "⣫", "⢝", "⢕", "⡑", "⠌", "⠂", "⠀"],
    interval: 180,
  },
} satisfies Record<string, { frames: string[]; interval: number }>;
