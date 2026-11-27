import { useState } from "react";
import { sessionRawFileUrl } from "../../lib/api";
import { isImagePath } from "../../lib/imagePath";
import { openInNewTab } from "../../lib/openInNewTab";
import { useArtifactObjectUrl } from "../../hooks/useArtifactObjectUrl";
import { Centered } from "./viewerChrome";
import { BUTTON_SECONDARY_SM } from "../../lib/surfaces";

function basename(filePath: string): string {
  return filePath.slice(filePath.lastIndexOf("/") + 1);
}

/** A binary session file: images inline, anything else as a download. */
export function BinaryFileView({ sessionId, filePath }: { sessionId: string; filePath: string }) {
  return isImagePath(filePath) ? (
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
        <span>Binary file</span>
        <button type="button" onClick={() => void download()} className={BUTTON_SECONDARY_SM}>
          Download
        </button>
        {failed && <span className="text-[12px] text-status-error-text">Download failed</span>}
      </div>
    </Centered>
  );
}
