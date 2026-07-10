import { EditorState, Prec } from "@codemirror/state";
import {
  EditorView,
  GutterMarker,
  gutter,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  drawSelection,
  dropCursor,
  crosshairCursor,
  rectangularSelection,
} from "@codemirror/view";
import { bracketMatching, defaultHighlightStyle, foldGutter, foldKeymap, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { closeBrackets, closeBracketsKeymap, autocompletion, completionKeymap } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { lintKeymap } from "@codemirror/lint";
import { highlightSelectionMatches as highlightSelectionMatchesInSearch, searchKeymap } from "@codemirror/search";
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

const rsduckEditorSetup = (onRun) => [
  runLineNumberGutter(onRun),
  highlightActiveLineGutter(),
  highlightSpecialChars(),
  history(),
  foldGutter(),
  drawSelection(),
  dropCursor(),
  EditorState.allowMultipleSelections.of(true),
  indentOnInput(),
  syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
  bracketMatching(),
  closeBrackets(),
  autocompletion(),
  rectangularSelection(),
  crosshairCursor(),
  highlightActiveLine(),
  highlightSelectionMatchesInSearch(),
  keymap.of([
    ...closeBracketsKeymap,
    ...defaultKeymap,
    ...searchKeymap,
    ...historyKeymap,
    ...foldKeymap,
    ...completionKeymap,
    ...lintKeymap,
  ]),
];

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
  ".cm-rsduckRunGutter .cm-gutterElement": {
    whiteSpace: "nowrap",
  },
  ".cm-rsduckRunGutter .cm-rsduck-run-line": {
    display: "inline-flex",
    alignItems: "center",
    justifyContent: "space-between",
    width: "100%",
    minWidth: "100%",
    gap: "4px",
    userSelect: "none",
  },
  ".cm-rsduckRunGutter .cm-rsduck-run-line .cm-rsduck-line-no": {
    minWidth: "2.2em",
    textAlign: "right",
    color: "#64748b",
  },
  ".cm-rsduckRunGutter .cm-rsduck-run-line .cm-rsduck-run-btn": {
    display: "inline-flex",
    alignItems: "center",
    justifyContent: "center",
    width: "14px",
    height: "14px",
    borderRadius: "4px",
    fontSize: "8px",
    border: "1px solid #b9c4d3",
    color: "#1d4ed8",
    background: "#f8fbff",
    cursor: "pointer",
  },
  ".cm-rsduckRunGutter .cm-rsduck-run-line .cm-rsduck-run-btn:hover": {
    background: "#eef3ff",
    borderColor: "#7aaef0",
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
  const selectedText = selectedSql(view);
  const sqlText = selectedText || view.state.doc.toString().trim();
  if (sqlText) onRun(sqlText, { selected: Boolean(selectedText) });
}

class RunLineNumberMarker extends GutterMarker {
  constructor(lineNumber, showButton, onRun) {
    super();
    this.lineNumber = lineNumber;
    this.showButton = showButton;
    this.onRun = onRun;
  }

  toDOM(view) {
    const root = document.createElement("span");
    root.className = "cm-rsduck-run-line";

    const number = document.createElement("span");
    number.className = "cm-rsduck-line-no";
    number.textContent = String(this.lineNumber);
    root.appendChild(number);

    if (this.showButton) {
      const button = document.createElement("span");
      button.className = "cm-rsduck-run-btn";
      button.textContent = "\u25B6";
      button.title = "Run current line";
      button.setAttribute("role", "button");
      button.setAttribute("aria-label", "Run current line");
      button.addEventListener("mousedown", (event) => {
        event.preventDefault();
        event.stopPropagation();
      });
      button.addEventListener("click", (event) => {
        event.preventDefault();
        event.stopPropagation();
        const line = view.state.doc.line(this.lineNumber);
        const sqlText = line.text.trim();
        if (sqlText) this.onRun(sqlText);
      });
      root.appendChild(button);
    }

    return root;
  }
}

function runLineNumberGutter(onRun) {
  return gutter({
    class: "cm-rsduckRunGutter",
    lineMarker(view, line) {
      const activeLine = view.state.doc.lineAt(view.state.selection.main.head).number;
      const lineNumber = view.state.doc.lineAt(line.from).number;
      return new RunLineNumberMarker(lineNumber, lineNumber === activeLine, onRun);
    },
    lineMarkerChange(update) {
      return update.selectionSet || update.docChanged;
    },
  });
}

function createRsduckSqlEditor(options) {
  const parent = options.parent;
  const initialDoc = options.initialDoc || "";
  const onRun = options.onRun || (() => {});

  const view = new EditorView({
    state: EditorState.create({
      doc: initialDoc,
      extensions: [
        ...rsduckEditorSetup(onRun),
        sql(),
        rsduckTheme,
        nativeSelectionTheme,
        runKeymap(onRun),
      ],
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
