/** Mirrors the server's `validate_profile_name`; returns an error message or null. */
export function validateProfileName(name: string): string | null {
  if (!name) return "Name is required";
  if (!/^[a-zA-Z0-9_-]+$/.test(name)) return "Only letters, digits, hyphens, and underscores";
  return null;
}
