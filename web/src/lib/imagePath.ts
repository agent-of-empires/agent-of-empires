// SVG is absent: the server never serves scriptable types inline.
const IMAGE_EXTENSIONS = new Set(["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico"]);

export function isImagePath(filePath: string): boolean {
  return IMAGE_EXTENSIONS.has(filePath.slice(filePath.lastIndexOf(".") + 1).toLowerCase());
}
