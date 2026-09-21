import type { ReactNode } from "react";
import { FileDown, Image as ImageIcon, Link as LinkIcon, Music } from "lucide-react";

import type { ToolOutputBlock } from "../../lib/acpTypes";
import { SAFE_LINK_SCHEMES, SAFE_MEDIA_SCHEMES, safeUri } from "./ToolCardChrome";

/** Structured completion payload rendered under any tool card. A block with no
 *  usable bytes or uri degrades to a labelled placeholder rather than vanishing. */
export function ToolOutputMedia({ blocks }: { blocks: ToolOutputBlock[] }) {
  return (
    <div className="my-1 space-y-2 overflow-hidden rounded-md border border-surface-700 bg-surface-800/50 p-3 text-sm">
      {blocks.map((block, i) => (
        <ToolOutputBlockView key={i} block={block} />
      ))}
    </div>
  );
}

const dataUri = (mimeType: string, data: string) => `data:${mimeType};base64,${data}`;
const ICON = "h-3.5 w-3.5";

function MediaPlaceholder({ icon, label }: { icon: ReactNode; label: string }) {
  return (
    <div className="flex items-center gap-2 text-[11px] text-text-dim italic">
      <span className="text-text-dim">{icon}</span>
      {label}
    </div>
  );
}

function TextPre({ text }: { text: string }) {
  return <pre className="whitespace-pre-wrap break-words font-mono text-xs text-text-secondary">{text}</pre>;
}

function LinkRow({ icon, label, ...anchor }: { icon: ReactNode; label: string } & React.ComponentProps<"a">) {
  return (
    <a {...anchor} className="flex items-center gap-2 text-xs text-accent-500 hover:underline">
      {icon}
      {label}
    </a>
  );
}

function ToolOutputBlockView({ block }: { block: ToolOutputBlock }) {
  switch (block.kind) {
    case "text":
      return <TextPre text={block.text} />;
    case "image": {
      const src = block.data
        ? dataUri(block.mime_type, block.data)
        : block.uri
          ? safeUri(block.uri, SAFE_MEDIA_SCHEMES)
          : null;
      if (!src) {
        return <MediaPlaceholder icon={<ImageIcon className={ICON} />} label={`image (${block.mime_type})`} />;
      }
      return (
        <img
          src={src}
          alt={`tool output image (${block.mime_type})`}
          className="max-h-80 max-w-full rounded border border-surface-700 object-contain"
        />
      );
    }
    case "audio":
      if (!block.data) {
        return <MediaPlaceholder icon={<Music className={ICON} />} label={`audio (${block.mime_type})`} />;
      }
      return <audio controls src={dataUri(block.mime_type, block.data)} className="w-full" />;
    case "resource_link": {
      const href = safeUri(block.uri, SAFE_LINK_SCHEMES);
      if (!href) {
        return <MediaPlaceholder icon={<LinkIcon className={ICON} />} label={`${block.name} (${block.uri})`} />;
      }
      return (
        <LinkRow href={href} target="_blank" rel="noreferrer" icon={<LinkIcon className={ICON} />} label={block.name} />
      );
    }
    case "resource": {
      if (block.text != null) return <TextPre text={block.text} />;
      // Blob bytes are offered as a download even when the uri is not fetchable.
      if (block.data) {
        const filename = block.uri.split("/").pop() || "resource";
        return (
          <LinkRow
            href={dataUri(block.mime_type ?? "application/octet-stream", block.data)}
            download={filename}
            icon={<FileDown className={ICON} />}
            label={filename}
          />
        );
      }
      const href = safeUri(block.uri, SAFE_LINK_SCHEMES);
      if (!href) return <MediaPlaceholder icon={<FileDown className={ICON} />} label={block.uri} />;
      return (
        <LinkRow href={href} target="_blank" rel="noreferrer" icon={<FileDown className={ICON} />} label={block.uri} />
      );
    }
    default:
      return null;
  }
}
