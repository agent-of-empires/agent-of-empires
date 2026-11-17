import type { SettingsFieldDescriptor } from "../../../lib/types";
import { SchemaSection } from "../../settings/SchemaSection";

interface Props {
  schema: SettingsFieldDescriptor[];
  values: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => Promise<boolean>;
}

const FIELDS = ["enabled", "path_template", "default_base_branch", "auto_cleanup"];

/** `{repo-name}` and `{branch}` filled in with sample values, so the template reads as a path. */
const examplePath = (template: string) =>
  template.replaceAll("{repo-name}", "my-app").replaceAll("{branch}", "fix-login");

export function WorktreesStep({ schema, values, onSaveField }: Props) {
  const template = typeof values.path_template === "string" ? values.path_template : "";
  return (
    <div className="flex flex-col gap-3">
      <div className="rounded-lg border border-surface-700 bg-surface-900 p-4">
        <SchemaSection
          section="worktree"
          schema={schema}
          values={values}
          onSaveField={onSaveField}
          onlyFields={FIELDS}
        />
      </div>
      {template && (
        <p className="rounded-md border border-dashed border-surface-700 px-3 py-2 font-mono text-xs text-text-dim">
          For my-app on <span className="text-accent-600">fix-login</span> → {examplePath(template)}
        </p>
      )}
    </div>
  );
}
