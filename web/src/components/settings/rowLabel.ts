/** Set when a `SettingRow` draws the label: the control then renders only its input, named by the row. */
export interface RowLabel {
  labelId: string;
  controlId: string;
}

/** The input's id and accessible name when a row labels it. */
export function rowInputProps(row: RowLabel | undefined) {
  return row ? { id: row.controlId, "aria-labelledby": row.labelId } : {};
}
