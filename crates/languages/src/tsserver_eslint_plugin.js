// A tsserver plugin that runs the project's ESLint inside tsserver, so JavaScript and TypeScript
// files don't need a separate ESLint language server process. Linting runs asynchronously after
// each change; diagnostics for the linted version are merged into tsserver's semantic diagnostics,
// and fixes into its code fixes.

// Outside the range TypeScript uses for its own diagnostic codes.
const ESLINT_DIAGNOSTIC_CODE = 990001;

function init(modules) {
  const ts = modules.typescript;

  function create(info) {
    const languageService = info.languageService;
    const project = info.project;
    const projectDirectory = project.getCurrentDirectory();
    const log = (message) =>
      project.projectService.logger.info(`[zed-tsserver-eslint] ${message}`);

    let eslintModule;
    try {
      eslintModule = require(
        require.resolve("eslint", { paths: [projectDirectory] }),
      );
    } catch (error) {
      log(`ESLint isn't installed in ${projectDirectory}: ${error.message}`);
      return languageService;
    }

    let eslint = null;
    // fileName -> { version, messages }
    const results = new Map();
    // fileName -> { running, requestedVersion, requestedText }
    const lints = new Map();

    const loadESLint =
      eslintModule.loadESLint ||
      (async () => eslintModule.ESLint || eslintModule.FlatESLint);
    loadESLint({ cwd: projectDirectory })
      .then((ESLint) => {
        eslint = new ESLint({ cwd: projectDirectory });
        project.refreshDiagnostics();
      })
      .catch((error) => log(`Couldn't load ESLint: ${error.stack || error}`));

    function requestLint(fileName, version, text) {
      let lint = lints.get(fileName);
      if (!lint) {
        lint = { running: false, requestedVersion: null, requestedText: null };
        lints.set(fileName, lint);
      }
      lint.requestedVersion = version;
      lint.requestedText = text;
      if (!lint.running) {
        runLint(fileName, lint);
      }
    }

    // Lints one version at a time per file; edits made meanwhile are linted next.
    async function runLint(fileName, lint) {
      lint.running = true;
      while (lint.requestedVersion !== null) {
        const version = lint.requestedVersion;
        const text = lint.requestedText;
        lint.requestedVersion = null;
        lint.requestedText = null;
        let messages = [];
        try {
          if (!(await eslint.isPathIgnored(fileName))) {
            const [result] = await eslint.lintText(text, { filePath: fileName });
            messages = result ? result.messages : [];
          }
        } catch (error) {
          log(`Linting ${fileName} failed: ${error.stack || error}`);
        }
        results.set(fileName, { version, messages });
      }
      lint.running = false;
      project.refreshDiagnostics();
    }

    function currentResults(fileName) {
      const cached = results.get(fileName);
      const version = info.languageServiceHost.getScriptVersion(fileName);
      // Results for an older version would point at the wrong text.
      return cached && cached.version === version ? cached : null;
    }

    function positionOf(sourceFile, line, column) {
      const lineStarts = sourceFile.getLineStarts();
      const lineIndex = Math.min(Math.max(line - 1, 0), lineStarts.length - 1);
      return Math.min(
        lineStarts[lineIndex] + Math.max(column - 1, 0),
        sourceFile.text.length,
      );
    }

    function toDiagnostic(sourceFile, message) {
      const start = positionOf(sourceFile, message.line, message.column);
      const end =
        message.endLine !== undefined
          ? positionOf(sourceFile, message.endLine, message.endColumn)
          : start;
      return {
        file: sourceFile,
        start,
        length: Math.max(end - start, 0),
        messageText: message.ruleId
          ? `${message.message} (${message.ruleId})`
          : message.message,
        category:
          message.severity === 2
            ? ts.DiagnosticCategory.Error
            : ts.DiagnosticCategory.Warning,
        code: ESLINT_DIAGNOSTIC_CODE,
        source: "eslint",
      };
    }

    function textChange(fileName, fix, description, fixName) {
      return {
        fixName,
        description,
        changes: [
          {
            fileName,
            textChanges: [
              {
                span: { start: fix.range[0], length: fix.range[1] - fix.range[0] },
                newText: fix.text,
              },
            ],
          },
        ],
      };
    }

    // Applies the fixes that don't overlap, in order, the way ESLint's own fixer does.
    function fixAll(fileName, messages) {
      const fixes = messages
        .filter((message) => message.fix)
        .map((message) => message.fix)
        .sort((left, right) => left.range[0] - right.range[0]);
      const textChanges = [];
      let lastEnd = -1;
      for (const fix of fixes) {
        if (fix.range[0] < lastEnd) {
          continue;
        }
        textChanges.push({
          span: { start: fix.range[0], length: fix.range[1] - fix.range[0] },
          newText: fix.text,
        });
        lastEnd = fix.range[1];
      }
      if (textChanges.length < 2) {
        return null;
      }
      return {
        fixName: "eslint-fix-all",
        description: "Fix all auto-fixable ESLint problems",
        changes: [{ fileName, textChanges }],
      };
    }

    const proxy = Object.create(null);
    for (const key of Object.keys(languageService)) {
      const member = languageService[key];
      proxy[key] =
        typeof member === "function" ? member.bind(languageService) : member;
    }

    proxy.getSemanticDiagnostics = (fileName) => {
      const diagnostics = languageService.getSemanticDiagnostics(fileName);
      if (!eslint) {
        return diagnostics;
      }
      const sourceFile = languageService.getProgram()?.getSourceFile(fileName);
      if (!sourceFile) {
        return diagnostics;
      }
      const cached = currentResults(fileName);
      if (!cached) {
        requestLint(
          fileName,
          info.languageServiceHost.getScriptVersion(fileName),
          sourceFile.text,
        );
        return diagnostics;
      }
      return diagnostics.concat(
        cached.messages.map((message) => toDiagnostic(sourceFile, message)),
      );
    };

    if (languageService.getSupportedCodeFixes) {
      proxy.getSupportedCodeFixes = (fileName) => [
        ...languageService.getSupportedCodeFixes(fileName),
        String(ESLINT_DIAGNOSTIC_CODE),
      ];
    }

    proxy.getCodeFixesAtPosition = (
      fileName,
      start,
      end,
      errorCodes,
      formatOptions,
      preferences,
    ) => {
      const typescriptCodes = errorCodes.filter(
        (code) => code !== ESLINT_DIAGNOSTIC_CODE,
      );
      const fixes =
        typescriptCodes.length > 0
          ? languageService.getCodeFixesAtPosition(
              fileName,
              start,
              end,
              typescriptCodes,
              formatOptions,
              preferences,
            )
          : [];
      const cached = currentResults(fileName);
      const sourceFile = languageService.getProgram()?.getSourceFile(fileName);
      if (!errorCodes.includes(ESLINT_DIAGNOSTIC_CODE) || !cached || !sourceFile) {
        return fixes;
      }
      const eslintFixes = [];
      for (const message of cached.messages) {
        const messageStart = positionOf(sourceFile, message.line, message.column);
        const messageEnd =
          message.endLine !== undefined
            ? positionOf(sourceFile, message.endLine, message.endColumn)
            : messageStart;
        if (messageEnd < start || messageStart > end) {
          continue;
        }
        if (message.fix) {
          eslintFixes.push(
            textChange(fileName, message.fix, `Fix: ${message.message}`, "eslint"),
          );
        }
        for (const suggestion of message.suggestions || []) {
          eslintFixes.push(
            textChange(fileName, suggestion.fix, suggestion.desc, "eslint-suggestion"),
          );
        }
        if (message.ruleId) {
          const lineStart = sourceFile.getLineStarts()[message.line - 1] ?? 0;
          const indentation = /^[ \t]*/.exec(sourceFile.text.slice(lineStart))[0];
          eslintFixes.push(
            textChange(
              fileName,
              {
                range: [lineStart, lineStart],
                text: `${indentation}// eslint-disable-next-line ${message.ruleId}\n`,
              },
              `Disable ${message.ruleId} for this line`,
              "eslint-disable",
            ),
          );
        }
      }
      const allFixes = fixAll(fileName, cached.messages);
      if (allFixes) {
        eslintFixes.push(allFixes);
      }
      return [...fixes, ...eslintFixes];
    };

    log(`Linting ${projectDirectory} with ESLint ${eslintModule.ESLint?.version ?? ""}`);
    return proxy;
  }

  return { create };
}

module.exports = init;
