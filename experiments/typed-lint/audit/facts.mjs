import { createRequire } from "node:module";
import { resolve } from "node:path";
import { compilerTextOffset, positions } from "../typescript.mjs";
import { fileIdentity, sourceLocations } from "../diagnostics/contract.mjs";

const require = createRequire(import.meta.url);

export function createAuditFacts(configPath, cwd) {
  let engine;
  let revision;
  let snapshot;
  let facts;
  return {
    get(next) {
      if (revision === next && facts) return facts;
      snapshot?.dispose();
      snapshot = facts = undefined;
      const invalidateAll = engine !== undefined;
      if (!engine) {
        const { API, SymbolFlags, TypeFlags } = require("tsrs/sync");
        const { SyntaxKind, NodeFlags } = require("tsrs/ast");
        engine = { api: new API({ cwd }), SymbolFlags, TypeFlags, SyntaxKind, NodeFlags, ast: require("tsrs/ast/is") };
      }
      snapshot = engine.api.createSnapshot({ openProjects: [configPath], fileNotifications: { invalidateAll } });
      const project = snapshot.getConfiguredProject(configPath);
      if (!project) throw new Error(`tsrs could not open ${configPath}`);
      facts = bindFacts(engine, project, cwd);
      revision = next;
      return facts;
    },
    close() {
      try {
        snapshot?.dispose();
      } finally {
        snapshot = facts = revision = undefined;
        const old = engine;
        engine = undefined;
        old?.api.close();
      }
    },
  };
}

function bindFacts({ api, ast, SymbolFlags, TypeFlags, SyntaxKind, NodeFlags }, { program, checker }, cwd) {
  const sources = sourceLocations(cwd);
  const docs = new Map();
  const symbols = new Map();
  const aliases = new Map();
  const definitions = new Map();
  const declarationFiles = new Map();
  const standardArrayMethods = new Map();
  const argumentTypes = new Map();
  const syntaxErrors = new Map();

  function doc(source) {
    const file = resolve(cwd, source.fileName);
    if (!docs.has(file)) {
      const bytes = sources.source(file);
      docs.set(file, { file, bytes, bom: bytes && compilerTextOffset(bytes, source.text) });
    }
    return docs.get(file);
  }

  function location(node) {
    const source = node.getSourceFile();
    const { file, bom } = doc(source);
    const start = source.getLineAndCharacterOfPosition(node.getStart(source));
    const end = source.getLineAndCharacterOfPosition(node.getEnd());
    const range = { start, end };
    const site = bom === undefined ? {} : sources.utf16(file, start.line + 1, start.character + 1, {
      endLine: end.line + 1, endColumn: end.character + 1, skipBom: bom === 3,
    });
    return { ...site, file, range };
  }

  function symbol(node) {
    if (!symbols.has(node.id)) symbols.set(node.id, checker.getSymbolAtLocation(node));
    return symbols.get(node.id);
  }

  function callee(node) {
    if (ast.isPropertyAccessExpression(node)) node = node.name;
    if (!ast.isIdentifier(node)) return;
    const binding = symbol(node);
    if (!binding || !(binding.flags & SymbolFlags.Alias)) return binding;
    if (!aliases.has(binding.id)) {
      const target = checker.getAliasedSymbol(binding);
      aliases.set(binding.id, checker.isUnknownSymbol(target) ? undefined : target);
    }
    return aliases.get(binding.id);
  }

  function definition(binding) {
    if (!binding) return;
    if (!definitions.has(binding.id)) {
      const body = binding.declarations
        .filter(({ kind }) => kind === SyntaxKind.FunctionDeclaration)
        .map((handle) => handle.resolve())
        .find((node) => node?.body);
      definitions.set(binding.id, body);
    }
    return definitions.get(binding.id);
  }

  function classify(type, flag) {
    if (type.isErrorType() || type.flags & (TypeFlags.AnyOrUnknown | TypeFlags.TypeParameter)) return undefined;
    if (type.flags & TypeFlags.Union) {
      const states = type.getTypes().map((part) => classify(part, flag));
      if (states.every((state) => state === true)) return true;
      if (states.every((state) => state === false)) return false;
      return undefined;
    }
    return Boolean(type.flags & flag);
  }

  return {
    ast, SyntaxKind, NodeFlags, sources, location, symbol, callee, definition,
    standardArrayMethod(binding) {
      if (binding?.name !== "forEach" && binding?.name !== "map") return;
      if (!standardArrayMethods.has(binding.id)) {
        const standard = binding.declarations.length > 0 && binding.declarations.every((handle) => {
          const declaration = handle.resolve();
          const owner = declaration?.parent;
          return owner && ast.isInterfaceDeclaration(owner) && ["Array", "ReadonlyArray"].includes(owner.name.text)
            && program.isSourceFileDefaultLibrary(declaration.getSourceFile());
        });
        standardArrayMethods.set(binding.id, standard ? binding.name : undefined);
      }
      return standardArrayMethods.get(binding.id);
    },
    hasSyntaxErrors(node) {
      const file = node.getSourceFile().fileName;
      if (!syntaxErrors.has(file)) syntaxErrors.set(file, program.getSyntacticDiagnostics(file).length > 0);
      return syntaxErrors.get(file);
    },
    prepare(fn, limit) {
      const nodes = [fn];
      const identifiers = [];
      let remaining = limit;
      while (nodes.length) {
        if (--remaining < 0) return false;
        const node = nodes.pop();
        if (ast.isIdentifier(node) && !symbols.has(node.id)) identifiers.push(node);
        if (node === fn || !ast.isFunctionLikeDeclaration(node)) {
          node.forEachChild((child) => { nodes.push(child); });
        }
      }
      for (let offset = 0; offset < identifiers.length; offset += 256) {
        const batch = identifiers.slice(offset, offset + 256);
        const resolved = checker.getSymbolAtLocation(batch);
        batch.forEach((node, index) => symbols.set(node.id, resolved[index]));
      }
      return true;
    },
    typeIs(node, type) {
      if (!argumentTypes.has(node.id)) argumentTypes.set(node.id, checker.getTypeAtLocation(node));
      const flag = { string: TypeFlags.StringLike, number: TypeFlags.NumberLike, boolean: TypeFlags.BooleanLike }[type];
      return classify(argumentTypes.get(node.id), flag);
    },
    declarationMatches(binding, { name, file }) {
      if (binding.name !== name) return false;
      if (!declarationFiles.has(binding.id)) {
        declarationFiles.set(binding.id, new Set(binding.declarations.flatMap((handle) => {
          const declaration = handle.resolve();
          return declaration ? [fileIdentity(declaration.getSourceFile().fileName, cwd)] : [];
        })));
      }
      return declarationFiles.get(binding.id).has(file);
    },
    candidates(matches) {
      const files = [...Map.groupBy(matches, ({ file }) => fileIdentity(file, cwd))];
      const spans = [];
      for (let index = 0; index < files.length; index += 128) {
        spans.push(...api.batch(...files.slice(index, index + 128).map(([file]) => program.getSourceFile.gen(file))));
      }
      return files.flatMap(([file, matches], index) => {
        const source = spans[index];
        const { bytes, bom } = source ? doc(source) : {};
        const offsets = bom === undefined ? undefined : positions(bytes,
          matches.flatMap(({ bytes, focus }) => [bytes.start, bytes.end, focus?.start, focus?.end].filter(Number.isInteger)), bom);
        const bySpan = new Map();
        const stack = source && offsets ? [source] : [];
        const wanted = new Set(matches.map(({ bytes }) => `${offsets?.get(bytes.start)}:${offsets?.get(bytes.end)}`));
        const ends = new Set(matches.filter(({ kind }) => kind === "entry").map(({ bytes }) => offsets?.get(bytes.end)));
        while (stack.length) {
          const node = stack.pop();
          if (ast.isFunctionDeclaration(node) || ast.isCallExpression(node)) {
            const end = node.getEnd();
            const key = `${node.getStart(source)}:${end}`;
            if (wanted.has(key)) bySpan.set(key, node);
            if (ast.isFunctionDeclaration(node) && node.modifiers?.length && ends.has(end)) {
              // Tree-sitter commonly puts export and declare in an outer node.
              const keyword = node.modifiers.find(({ kind }) => kind === SyntaxKind.AsyncKeyword)
                ?? node.getChildren(source).find(({ kind }) => kind === SyntaxKind.FunctionKeyword);
              const inner = `${keyword?.getStart(source)}:${end}`;
              if (wanted.has(inner)) bySpan.set(inner, node);
            }
          }
          node.forEachChild((child) => { stack.push(child); });
        }
        return matches.map((match) => ({
          ...match, file,
          node: bySpan.get(`${offsets?.get(match.bytes.start)}:${offsets?.get(match.bytes.end)}`),
          focusOffsets: match.focus && offsets && [offsets.get(match.focus.start), offsets.get(match.focus.end)],
        }));
      });
    },
  };
}
