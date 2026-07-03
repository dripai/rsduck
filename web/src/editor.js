import { EditorState, Prec } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { sql } from "@codemirror/lang-sql";

const runKeymap = (onRun) =>
  keymap.of([
    {
      key: "Ctrl-Enter",
      run(view) {
        runFromView(view, onRun);
        return true;
      },
    },
    {
      key: "Mod-Enter",
      run(view) {
        runFromView(view, onRun);
        return true;
      },
    },
  ]);

const rsduckTheme = EditorView.theme({
  "&": {
    height: "100%",
    fontSize: "14px",
    backgroundColor: "#fff",
    color: "#0f172a",
  },
  ".cm-scroller": {
    fontFamily: 'Consolas, "Courier New", monospace',
    lineHeight: "1.55",
  },
  ".cm-content": {
    padding: "12px 0",
  },
  ".cm-line": {
    padding: "0 14px",
  },
  ".cm-gutters": {
    backgroundColor: "#f8fafc",
    color: "#64748b",
    borderRight: "1px solid #d8dde6",
  },
  ".cm-activeLine": {
    backgroundColor: "#f3f7ff",
  },
  ".cm-activeLineGutter": {
    backgroundColor: "#e8f2ff",
    color: "#1d4ed8",
  },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": {
    backgroundColor: "#7fb3ff !important",
  },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground": {
    backgroundColor: "#7fb3ff !important",
  },
  ".cm-cursor": {
    borderLeftColor: "#0f172a",
  },
  ".cm-tooltip": {
    borderColor: "#b9c4d3",
    boxShadow: "0 8px 24px rgba(15, 23, 42, .18)",
  },
});

const nativeSelectionTheme = Prec.highest(
  EditorView.theme({
    ".cm-line": {
      "&::selection, & ::selection": {
        backgroundColor: "#7fb3ff !important",
        color: "inherit !important",
      },
    },
    ".cm-content": {
      "&::selection, & ::selection": {
        backgroundColor: "#7fb3ff !important",
        color: "inherit !important",
      },
      "& :focus": {
        "&::selection, & ::selection": {
          backgroundColor: "#7fb3ff !important",
          color: "inherit !important",
        },
      },
    },
  }),
);

function selectedSql(view) {
  return view.state.selection.ranges
    .filter((range) => !range.empty)
    .map((range) => view.state.sliceDoc(range.from, range.to).trim())
    .filter(Boolean)
    .join("\n");
}

function runFromView(view, onRun) {
  const sqlText = selectedSql(view) || view.state.doc.toString().trim();
  if (sqlText) onRun(sqlText);
}

function createRsduckSqlEditor(options) {
  const parent = options.parent;
  const initialDoc = options.initialDoc || "";
  const onRun = options.onRun || (() => {});

  const view = new EditorView({
    state: EditorState.create({
      doc: initialDoc,
      extensions: [basicSetup, sql(), rsduckTheme, nativeSelectionTheme, runKeymap(onRun)],
    }),
    parent,
  });

  return {
    view,
    getValue() {
      return view.state.doc.toString();
    },
    setValue(value) {
      view.dispatch({
        changes: {
          from: 0,
          to: view.state.doc.length,
          insert: value || "",
        },
      });
    },
    getSelectedText() {
      return selectedSql(view);
    },
    focus() {
      view.focus();
    },
  };
}

window.RsduckEditor = {
  create: createRsduckSqlEditor,
};
