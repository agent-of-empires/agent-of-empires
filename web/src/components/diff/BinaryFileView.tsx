import { useState } from "react";
import { sessionRawFileUrl } from "../../lib/api";
import { openInNewTab } from "../../lib/openInNewTab";
import { useArtifactObjectUrl } from "../../hooks/useArtifactObjectUrl";
import { Centered } from "./viewerChrome";

// SVG is absent: the server never serves scriptable types inline.
const IMAGE_EXTENSIONS = new Set(["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico"]);

function isImage(filePath: string): boolean {
  return IMAGE_EXTENSIONS.has(filePath.slice(filePath.lastIndexOf(".") + 1).toLowerCase());
}

function basename(filePath: string): string {
  return filePath.slice(filePath.lastIndexOf("/") + 1);
}

/** A binary session file: images inline, anything else as a download. */
export function BinaryFileView({ sessionId, filePath }: { sessionId: string; filePath: string }) {
  return isImage(filePath) ? (
    <ImageView key={`${sessionId} ${filePath}`} sessionId={sessionId} filePath={filePath} />
  ) : (
    <DownloadView sessionId={sessionId} filePath={filePath} />
  );
}

function ImageView({ sessionId, filePath }: { sessionId: string; filePath: string }) {
  const [undecodable, setUndecodable] = useState(false);
  // The raw route needs the token a bare <img src> lacks.
  const objectUrl = useArtifactObjectUrl(sessionRawFileUrl(sessionId, filePath));
  if (!objectUrl || undecodable) return <DownloadView sessionId={sessionId} filePath={filePath} />;
  return (
    <div className="flex-1 overflow-auto p-4 flex items-start justify-center">
      <img
        src={objectUrl}
        alt={basename(filePath)}
        className="max-w-full h-auto"
        onError={() => setUndecodable(true)}
      />
    </div>
  );
}

function DownloadView({ sessionId, filePath }: { sessionId: string; filePath: string }) {
  const [failed, setFailed] = useState(false);
  const download = async () => {
    const result = await openInNewTab(sessionRawFileUrl(sessionId, filePath), basename(filePath), true);
    setFailed(!result.ok);
  };
  return (
    <Centered>
      <div className="flex flex-col items-center gap-2">
        <span className="text-sm">Binary file</span>
        <button
          type="button"
          onClick={() => void download()}
          className="px-3 py-1 text-[12px] rounded border border-surface-700/40 text-text-secondary hover:text-text-primary cursor-pointer transition-colors"
        >
          Download
        </button>
        {failed && <span className="text-xs text-status-error">Download failed</span>}
      </div>
    </Centered>
  );
}
