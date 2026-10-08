// Small line icons for the desktop app, drawn inline (the page's CSP allows
// no external images or fonts). 24-unit grid, stroked in the text color.
import type { ReactNode } from "react";

function Svg({ children, size = 16 }: { children: ReactNode; size?: number }) {
  return (
    <svg
      className="icon"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

/** Anchor's mark: an anchor. */
export function Logo({ size = 20 }: { size?: number }) {
  return (
    <Svg size={size}>
      <circle cx="12" cy="5" r="2.2" />
      <path d="M12 7.2V21" />
      <path d="M8 11h8" />
      <path d="M4.5 14.5c.6 3.6 3.6 6.5 7.5 6.5s6.9-2.9 7.5-6.5" />
      <path d="M3 15.5l1.5-1.5 1.5 1.5M18 15.5l1.5-1.5 1.5 1.5" />
    </Svg>
  );
}

export const Plus = () => (
  <Svg>
    <path d="M12 5v14M5 12h14" />
  </Svg>
);
export const History = () => (
  <Svg>
    <path d="M3 12a9 9 0 1 0 3-6.7L3 8" />
    <path d="M3 3v5h5M12 7v5l3 2" />
  </Svg>
);
export const Folder = () => (
  <Svg>
    <path d="M3 6.5A1.5 1.5 0 0 1 4.5 5H9l2 2h8.5A1.5 1.5 0 0 1 21 8.5v9a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 17.5z" />
  </Svg>
);
export const FolderPlus = () => (
  <Svg>
    <path d="M3 6.5A1.5 1.5 0 0 1 4.5 5H9l2 2h8.5A1.5 1.5 0 0 1 21 8.5v9a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 17.5z" />
    <path d="M12 10.5v5M9.5 13h5" />
  </Svg>
);
export const Gear = () => (
  <Svg>
    <circle cx="12" cy="12" r="3" />
    <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" />
  </Svg>
);
export const Send = () => (
  <Svg>
    <path d="M5 12h14M13 6l6 6-6 6" />
  </Svg>
);
export const Stop = () => (
  <Svg>
    <rect x="7" y="7" width="10" height="10" rx="1.5" />
  </Svg>
);
export const Close = () => (
  <Svg>
    <path d="M6 6l12 12M18 6L6 18" />
  </Svg>
);
export const Chevron = ({ open = false }: { open?: boolean }) => (
  <Svg size={14}>{open ? <path d="M6 9l6 6 6-6" /> : <path d="M9 6l6 6-6 6" />}</Svg>
);
export const PanelLeft = () => (
  <Svg>
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <path d="M9 4v16" />
  </Svg>
);
export const Warning = () => (
  <Svg>
    <path d="M12 3l9.5 17h-19z" />
    <path d="M12 10v4M12 17.5v.01" />
  </Svg>
);
export const Shield = () => (
  <Svg>
    <path d="M12 3l8 3v6c0 4.5-3.4 8.2-8 9-4.6-.8-8-4.5-8-9V6z" />
    <path d="M9 12l2 2 4-4" />
  </Svg>
);
export const Camera = () => (
  <Svg>
    <path d="M4 8h3l2-3h6l2 3h3v11H4z" />
    <circle cx="12" cy="13" r="3.5" />
  </Svg>
);
export const ListIcon = () => (
  <Svg>
    <path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" />
  </Svg>
);
export const Coins = () => (
  <Svg>
    <ellipse cx="12" cy="6" rx="7" ry="3" />
    <path d="M5 6v6c0 1.7 3.1 3 7 3s7-1.3 7-3V6M5 12v6c0 1.7 3.1 3 7 3s7-1.3 7-3v-6" />
  </Svg>
);
export const Cpu = () => (
  <Svg size={14}>
    <rect x="6" y="6" width="12" height="12" rx="2" />
    <path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4" />
  </Svg>
);
