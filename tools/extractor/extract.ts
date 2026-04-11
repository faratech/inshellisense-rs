/*!
 * insh-rs spec extractor.
 *
 * Walks @withfig/autocomplete/src/*.ts, parses each file with ts-morph,
 * locates the default-exported Fig.Spec object literal, and converts the
 * pure-data subset into JSON matching insh-rs's Rust schema.
 *
 * Functions (postProcess callbacks, custom generators, generateSpec, etc.)
 * are NOT extracted in this pass — a spec containing any function at any
 * depth below a supported field is classified as "partial" or "js_only"
 * and skipped. Phase 6 will handle those via rquickjs.
 *
 * Output: one .json file per pure spec into <out>/essentials/<name>.json.
 * A manifest index.json enumerates everything extracted with kind tags.
 *
 * Usage:
 *   SRC=/tmp/withfig-autocomplete/src OUT=../../specs-data npm run extract
 */

import * as fs from "node:fs";
import * as path from "node:path";
import {
  ArrayLiteralExpression,
  ArrowFunction,
  FunctionExpression,
  Node,
  ObjectLiteralExpression,
  Project,
  PropertyAssignment,
  SourceFile,
  SyntaxKind,
} from "ts-morph";

const SRC = process.env.SRC ?? "/tmp/withfig-autocomplete/src";
const OUT = process.env.OUT ?? path.resolve(process.cwd(), "../../specs-data");
const EMBED_DIR = path.join(OUT, "embed");
const EXTRAS_DIR = path.join(OUT, "extras");

// Priority set: the top commands we want to ensure get extracted. We still
// try to extract everything pure, but these are the ones most likely to be
// used interactively.
const ESSENTIALS = new Set([
  "git", "docker", "kubectl", "ssh", "cargo", "npm", "apt", "systemctl",
  "curl", "find", "grep", "sed", "ls", "cp", "mv", "tar", "make", "python",
  "node", "go", "rustup", "gh", "pnpm", "yarn", "bun", "helm", "terraform",
  "rg", "fd", "bat", "exa", "eza", "fzf", "tmux", "vim", "nvim", "emacs",
  "ps", "top", "htop", "kill", "chmod", "chown", "df", "du", "free",
  "systemctl", "journalctl", "ip", "ss", "dig", "nc", "nmap", "openssl",
  "ffmpeg", "yt-dlp", "zip", "unzip", "awk", "jq", "yq", "wget",
]);

interface Stats {
  total: number;
  pure: number;
  partial: number;
  js_only: number;
  error: number;
  essentials_extracted: number;
  function_skips: number;
}

const stats: Stats = {
  total: 0,
  pure: 0,
  partial: 0,
  js_only: 0,
  error: 0,
  essentials_extracted: 0,
  function_skips: 0,
};

interface ManifestEntry {
  name: string;
  file: string;
  kind: "pure" | "partial" | "js_only";
  has_functions: boolean;
}

const manifest: ManifestEntry[] = [];

// ---------------------------------------------------------------------------
// Value extraction
// ---------------------------------------------------------------------------

type ExtractCtx = {
  has_functions: boolean;
  file: string;
  /// When true, the classifier is evaluating a function expression in the
  /// context of a generator's `postProcess` field — recognized shapes are
  /// converted to PostProcessKind variants instead of flagging the spec.
  post_process_in_progress: boolean;
};

/// Attempt to classify a function expression used as a postProcess
/// callback into a named PostProcessKind.  Returns the kind descriptor
/// on a hit, null on a miss (caller should fall back to marking the
/// containing spec as having functions).
/// Recognize calls to @fig/autocomplete-generators helpers and emit the
/// Rust-native generator descriptor directly.
function classifyFigHelper(name: string, args: Node[]): any | null {
  switch (name) {
    case "filepaths": {
      // filepaths() or filepaths({ extensions: [...], ... })
      // We emit a Template::Filepaths generator. Extension filtering
      // requires runtime support that isn't wired yet, so we just
      // produce the plain filepaths template.
      return { kind: "template", template: "filepaths" };
    }
    case "folders": {
      return { kind: "template", template: "folders" };
    }
    case "keyValue":
    case "keyValueList": {
      // These produce key/value pairs from a script. Without the script
      // argument being statically extractable we can't emit a generator.
      // Conservative: return null so the arg still extracts without it.
      return null;
    }
    default:
      return null;
  }
}

/// Attempt to classify a function expression used as a postProcess
/// callback into a named PostProcessKind. Returns the kind descriptor
/// on a hit, null on a miss (caller should fall back to marking the
/// containing spec as having functions).
function classifyPostProcess(
  fn: ArrowFunction | FunctionExpression
): any | null {
  // Target shape: (out) => out.split("sep").map(...)  OR the one-statement
  // return form. Everything is done syntactically; we don't evaluate.
  const body = fn.getBody();
  let expr: Node | undefined;
  if (Node.isBlock(body)) {
    // Single return statement?
    const stmts = body.getStatements();
    if (stmts.length === 1 && Node.isReturnStatement(stmts[0])) {
      expr = stmts[0].getExpression();
    } else {
      return null;
    }
  } else {
    expr = body;
  }
  if (!expr) return null;

  // Pattern: X.split("sep").map(arrow) where X is the first parameter.
  // The arrow body can be { name: line } or { name: line, description: ... }
  // — we match shallowly.
  if (!Node.isCallExpression(expr)) return null;
  const mapCall = expr;
  const mapAccess = mapCall.getExpression();
  if (!Node.isPropertyAccessExpression(mapAccess)) return null;
  if (mapAccess.getName() !== "map") return null;
  const splitCall = mapAccess.getExpression();
  if (!Node.isCallExpression(splitCall)) return null;
  const splitAccess = splitCall.getExpression();
  if (!Node.isPropertyAccessExpression(splitAccess)) return null;
  if (splitAccess.getName() !== "split") return null;

  // The split target must reference the function's first parameter.
  // Relaxed check: we just need the target to be an Identifier.
  if (!Node.isIdentifier(splitAccess.getExpression())) return null;

  // Split separator must be a string literal (commonly "\n").
  const splitArgs = splitCall.getArguments();
  if (splitArgs.length !== 1) return null;
  if (
    !Node.isStringLiteral(splitArgs[0]) &&
    !Node.isNoSubstitutionTemplateLiteral(splitArgs[0])
  )
    return null;

  // Map callback is an arrow returning an object literal with `name` set
  // to an identifier (the iteration variable). We accept this as SplitLines.
  const mapArgs = mapCall.getArguments();
  if (mapArgs.length !== 1) return null;
  const mapArg = mapArgs[0];
  if (!Node.isArrowFunction(mapArg) && !Node.isFunctionExpression(mapArg))
    return null;
  const mapBody = mapArg.getBody();
  let mapExpr: Node | undefined;
  if (Node.isBlock(mapBody)) {
    const stmts = mapBody.getStatements();
    if (stmts.length === 1 && Node.isReturnStatement(stmts[0])) {
      mapExpr = stmts[0].getExpression();
    } else {
      return null;
    }
  } else {
    mapExpr = mapBody;
  }
  // `(x) => ({...})` wraps the object literal in parens. Unwrap.
  while (mapExpr && Node.isParenthesizedExpression(mapExpr)) {
    mapExpr = mapExpr.getExpression();
  }
  if (!mapExpr || !Node.isObjectLiteralExpression(mapExpr)) return null;
  const mapObj = mapExpr;
  const props = mapObj.getProperties();
  if (props.length === 0) return null;

  // At least one property named `name` must be present.
  const hasName = props.some(
    (p) => Node.isPropertyAssignment(p) && p.getName() === "name"
  );
  if (!hasName) return null;

  // Property values must be "tolerable": identifiers, string/number
  // literals, template literals, property-access, or type-assertion
  // wrappers. Anything else (conditional, regex, nested call) bails.
  // Template literals are allowed even with interpolations — we drop
  // those fields at runtime; the `name` field is the only one we need
  // to be correct, and we already verified it's present above.
  for (const p of props) {
    if (!Node.isPropertyAssignment(p)) return null;
    const v = p.getInitializer();
    if (!v) return null;
    if (
      !Node.isIdentifier(v) &&
      !Node.isStringLiteral(v) &&
      !Node.isNoSubstitutionTemplateLiteral(v) &&
      !Node.isTemplateExpression(v) &&
      !Node.isPropertyAccessExpression(v) &&
      !Node.isNumericLiteral(v) &&
      !Node.isElementAccessExpression(v)
    ) {
      return null;
    }
  }

  // Emit SplitLines DSL descriptor.
  return { kind: "pattern", inner: { kind: "split_lines" } };
}

/// Second postProcess pattern: JSON.parse(out) + Object.keys(...).map(...)
/// or array.map(...) shape. Emits JsonParse.
function classifyJsonParsePostProcess(
  fn: ArrowFunction | FunctionExpression
): any | null {
  const body = fn.getBody();
  if (!Node.isBlock(body)) return null;
  const stmts = body.getStatements();
  // Expect: const x = JSON.parse(out); return ...;
  if (stmts.length < 2) return null;
  const firstStmt = stmts[0];
  if (!Node.isVariableStatement(firstStmt)) return null;
  const decl = firstStmt.getDeclarationList().getDeclarations()[0];
  if (!decl) return null;
  const init = decl.getInitializer();
  if (!init || !Node.isCallExpression(init)) return null;
  const callee = init.getExpression();
  if (!Node.isPropertyAccessExpression(callee)) return null;
  if (callee.getName() !== "parse") return null;
  const obj = callee.getExpression();
  if (!Node.isIdentifier(obj) || obj.getText() !== "JSON") return null;
  // Got `const x = JSON.parse(out);`. Last statement should be a return.
  const lastStmt = stmts[stmts.length - 1];
  if (!Node.isReturnStatement(lastStmt)) return null;
  // Success — emit JsonParse.
  return { kind: "pattern", inner: { kind: "json_parse" } };
}

function extractValue(node: Node | undefined, ctx: ExtractCtx): any {
  if (!node) return null;

  if (Node.isStringLiteral(node) || Node.isNoSubstitutionTemplateLiteral(node)) {
    return node.getLiteralText();
  }
  if (Node.isTemplateExpression(node)) {
    // Non-literal template — mark as function since we can't reliably
    // stringify it without interpolation.
    ctx.has_functions = true;
    return null;
  }
  if (Node.isNumericLiteral(node)) {
    return Number(node.getText());
  }
  if (node.getKind() === SyntaxKind.TrueKeyword) return true;
  if (node.getKind() === SyntaxKind.FalseKeyword) return false;
  if (node.getKind() === SyntaxKind.NullKeyword) return null;
  if (node.getKind() === SyntaxKind.UndefinedKeyword) return null;

  if (Node.isArrayLiteralExpression(node)) {
    const arr = node as ArrayLiteralExpression;
    return arr.getElements().map((e) => extractValue(e, ctx));
  }
  if (Node.isObjectLiteralExpression(node)) {
    return extractObject(node, ctx);
  }
  if (Node.isIdentifier(node)) {
    // Resolve the identifier to its declaration. If it points at a
    // top-level const with an initializer we can extract, treat the
    // reference as if it were the initializer inlined.
    const sym = node.getSymbol();
    if (sym) {
      const aliased = sym.getAliasedSymbol?.();
      const target = aliased ?? sym;
      for (const decl of target.getDeclarations()) {
        if (Node.isVariableDeclaration(decl)) {
          const init = decl.getInitializer();
          if (init) {
            // Recurse. If the initializer contains functions, the ctx
            // flag flips naturally.
            return extractValue(init, ctx);
          }
        }
      }
    }
    ctx.has_functions = true;
    return null;
  }
  if (Node.isFunctionExpression(node) || Node.isArrowFunction(node)) {
    // If we're inside a generator's postProcess field, try to classify.
    if (ctx.post_process_in_progress) {
      const fn = node as ArrowFunction | FunctionExpression;
      const splitKind = classifyPostProcess(fn);
      if (splitKind) return { __post_process_kind: splitKind };
      const jsonKind = classifyJsonParsePostProcess(fn);
      if (jsonKind) return { __post_process_kind: jsonKind };
    }
    ctx.has_functions = true;
    return { __fn: true };
  }
  if (Node.isMethodDeclaration(node)) {
    ctx.has_functions = true;
    return { __fn: true };
  }
  if (Node.isAsExpression(node) || Node.isParenthesizedExpression(node)) {
    return extractValue(node.getExpression(), ctx);
  }
  if (Node.isSpreadElement(node)) {
    // Spread of an unresolvable expression → mark impure.
    ctx.has_functions = true;
    return null;
  }
  if (Node.isPropertyAccessExpression(node)) {
    // Resolve `obj.prop` — look up obj's declaration, navigate the
    // declared object literal for `prop`. Handles patterns like
    // `sharedCommands.run` and `gitGenerators.commits`.
    const obj = node.getExpression();
    const propName = node.getName();
    if (Node.isIdentifier(obj)) {
      const sym = obj.getSymbol();
      if (sym) {
        const target = sym.getAliasedSymbol?.() ?? sym;
        for (const decl of target.getDeclarations()) {
          if (Node.isVariableDeclaration(decl)) {
            const init = decl.getInitializer();
            if (init && Node.isObjectLiteralExpression(init)) {
              for (const p of init.getProperties()) {
                if (Node.isPropertyAssignment(p) && p.getName() === propName) {
                  return extractValue(p.getInitializer(), ctx);
                }
              }
            }
          }
        }
      }
    }
    ctx.has_functions = true;
    return null;
  }
  if (Node.isCallExpression(node)) {
    // Known @fig/autocomplete-generators helpers that we hand-port in
    // Rust to first-class Template/Generator variants.
    const callee = node.getExpression();
    if (Node.isIdentifier(callee)) {
      const name = callee.getText();
      const helper = classifyFigHelper(name, node.getArguments());
      if (helper) return helper;
    }
    // Any other call result is not statically knowable.
    ctx.has_functions = true;
    return null;
  }

  // Unknown node shape — conservative: mark impure.
  ctx.has_functions = true;
  return null;
}

/// Fields where individual element failures are tolerable — we drop the
/// impure element and keep the rest. This is what lets specs survive when
/// e.g. one `custom` generator is complex but their sibling static options
/// are fine.
const TOLERANT_LIST_FIELDS = new Set([
  "subcommands",
  "options",
  "args",
  "generators",
  "suggestions",
]);

/// Extract a value with an "isolation barrier" — if anything in its
/// subtree sets has_functions, the flag is reset and the function returns
/// null instead of polluting the outer ctx.
function extractValueIsolated(node: Node | undefined, ctx: ExtractCtx): any {
  const saved = ctx.has_functions;
  ctx.has_functions = false;
  const v = extractValue(node, ctx);
  const wasImpure = ctx.has_functions;
  ctx.has_functions = saved;
  if (wasImpure) return null;
  return v;
}

function extractObject(obj: ObjectLiteralExpression, ctx: ExtractCtx): any {
  const out: Record<string, any> = {};
  for (const prop of obj.getProperties()) {
    if (Node.isPropertyAssignment(prop)) {
      const pa = prop as PropertyAssignment;
      const name = pa.getName();
      const isPostProcess = name === "postProcess";
      const savedFlag = ctx.post_process_in_progress;
      if (isPostProcess) ctx.post_process_in_progress = true;

      // Tolerant-list field: extract each element in isolation and drop
      // failures. If the init is a single (non-array) value, isolate at
      // the whole-field level.
      if (TOLERANT_LIST_FIELDS.has(name)) {
        const init = pa.getInitializer();
        if (init && Node.isArrayLiteralExpression(init)) {
          const arr: any[] = [];
          for (const el of init.getElements()) {
            const v = extractValueIsolated(el, ctx);
            if (v !== null && v !== undefined) arr.push(v);
          }
          out[name] = arr;
        } else {
          const v = extractValueIsolated(init, ctx);
          if (v !== null && v !== undefined) {
            out[name] = v;
          }
        }
      } else {
        const value = extractValue(pa.getInitializer(), ctx);
        if (value !== undefined) {
          out[name] = value;
        }
      }

      if (isPostProcess) ctx.post_process_in_progress = savedFlag;
    } else if (Node.isShorthandPropertyAssignment(prop)) {
      ctx.has_functions = true;
    } else if (Node.isSpreadAssignment(prop)) {
      ctx.has_functions = true;
    } else if (Node.isMethodDeclaration(prop)) {
      ctx.has_functions = true;
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// Shape conversion: upstream Fig.Spec → insh-rs Rust schema
// ---------------------------------------------------------------------------

function toRustSubcommand(x: any): any {
  if (!x || typeof x !== "object") return null;
  const out: any = {};
  // names: string | string[]
  if (typeof x.name === "string") out.names = [x.name];
  else if (Array.isArray(x.name)) out.names = x.name.filter((n: any) => typeof n === "string");
  else return null; // no name = not a valid subcommand

  if (out.names.length === 0) return null;

  if (typeof x.description === "string") out.description = x.description;
  if (typeof x.displayName === "string") out.display_name = x.displayName;
  if (typeof x.priority === "number") out.priority = clamp0_100(x.priority);
  if (x.hidden === true) out.hidden = true;
  if (x.isDangerous === true) out.is_dangerous = true;
  if (x.deprecated === true) out.deprecated = true;
  if (x.requiresSubcommand === true) out.requires_subcommand = true;
  if (typeof x.icon === "string") out.icon = x.icon;

  if (Array.isArray(x.subcommands)) {
    out.subcommands = x.subcommands
      .map(toRustSubcommand)
      .filter((v: any) => v !== null);
  }
  if (Array.isArray(x.options)) {
    out.options = x.options.map(toRustOpt).filter((v: any) => v !== null);
  }
  const argsVal = toRustArgsField(x.args);
  if (argsVal !== null) out.args = argsVal;

  // loadSpec: string only for now (functions become js).
  if (typeof x.loadSpec === "string") {
    out.load_spec = { kind: "spec_path", name: x.loadSpec };
  }

  // parserDirectives
  if (x.parserDirectives && typeof x.parserDirectives === "object") {
    const pd: any = {};
    if (x.parserDirectives.flagsArePosixNoncompliant === true)
      pd.flags_are_posix_noncompliant = true;
    if (x.parserDirectives.optionsMustPrecedeArguments === true)
      pd.options_must_precede_arguments = true;
    if (Array.isArray(x.parserDirectives.optionArgSeparators))
      pd.option_arg_separators = x.parserDirectives.optionArgSeparators.filter(
        (s: any) => typeof s === "string"
      );
    if (Object.keys(pd).length > 0) out.parser_directives = pd;
  }

  return out;
}

function toRustOpt(x: any): any {
  if (!x || typeof x !== "object") return null;
  const out: any = {};
  if (typeof x.name === "string") out.names = [x.name];
  else if (Array.isArray(x.name)) out.names = x.name.filter((n: any) => typeof n === "string");
  else return null;
  if (out.names.length === 0) return null;

  if (typeof x.description === "string") out.description = x.description;
  if (typeof x.displayName === "string") out.display_name = x.displayName;
  if (typeof x.priority === "number") out.priority = clamp0_100(x.priority);
  if (x.hidden === true) out.hidden = true;
  if (x.deprecated === true) out.deprecated = true;
  if (x.isPersistent === true) out.is_persistent = true;
  if (x.isRequired === true) out.is_required = true;
  if (x.isRepeatable === true) out.is_repeatable = true;
  else if (x.isRepeatable === false) out.is_repeatable = false;
  else if (typeof x.isRepeatable === "number") out.is_repeatable = x.isRepeatable;
  if (Array.isArray(x.exclusiveOn))
    out.exclusive_on = x.exclusiveOn.filter((s: any) => typeof s === "string");
  if (Array.isArray(x.dependsOn))
    out.depends_on = x.dependsOn.filter((s: any) => typeof s === "string");
  if (typeof x.requiresSeparator === "string") out.requires_separator = x.requiresSeparator;

  const argsVal = toRustArgsField(x.args);
  if (argsVal !== null) out.args = argsVal;

  return out;
}

function toRustArgsField(x: any): any[] | null {
  if (x == null) return null;
  const arr = Array.isArray(x) ? x : [x];
  const out = arr.map(toRustArg).filter((v: any) => v !== null);
  return out.length > 0 ? out : null;
}

function toRustArg(x: any): any {
  if (!x || typeof x !== "object") return null;
  const out: any = {};
  if (typeof x.name === "string") out.name = x.name;
  if (typeof x.description === "string") out.description = x.description;
  if (x.isOptional === true) out.is_optional = true;
  if (x.isVariadic === true) out.is_variadic = true;
  if (x.isCommand === true) out.is_command = true;
  if (x.isScript === true) out.is_script = true;
  if (x.debounce === true) out.debounce = true;
  if (typeof x.default === "string") out.default = x.default;

  // template: "filepaths" | "folders" | "history" | "help" | array
  const tplsField = x.template;
  if (tplsField != null) {
    const tpls = Array.isArray(tplsField) ? tplsField : [tplsField];
    const mapped = tpls
      .map((t: any) => (typeof t === "string" ? mapTemplate(t) : null))
      .filter((v: any) => v !== null);
    if (mapped.length > 0) out.templates = mapped;
  }

  // suggestions: (string | Suggestion)[]
  if (Array.isArray(x.suggestions)) {
    const sugs: any[] = [];
    for (const s of x.suggestions) {
      if (typeof s === "string") {
        sugs.push({ name: s });
      } else if (s && typeof s === "object") {
        const sug: any = {};
        if (typeof s.name === "string") sug.name = s.name;
        else if (Array.isArray(s.name)) {
          sug.name = s.name[0];
          sug.all_names = s.name;
        } else continue;
        if (typeof s.description === "string") sug.description = s.description;
        if (typeof s.icon === "string") sug.icon = s.icon;
        if (typeof s.priority === "number") sug.priority = clamp0_100(s.priority);
        if (typeof s.insertValue === "string") sug.insert_value = s.insertValue;
        if (typeof s.displayName === "string") sug.display_name = s.displayName;
        sugs.push(sug);
      }
    }
    if (sugs.length > 0) out.suggestions = sugs;
  }

  // generators: Generator | Generator[]
  const gens = x.generators;
  if (gens != null) {
    const arr = Array.isArray(gens) ? gens : [gens];
    const mapped = arr.map(toRustGenerator).filter((v: any) => v !== null);
    if (mapped.length > 0) out.generators = mapped;
  }

  // filterStrategy
  if (typeof x.filterStrategy === "string") {
    out.filter_strategy = x.filterStrategy; // serde handles snake_case mapping
  }

  return out;
}

function toRustGenerator(g: any): any {
  if (!g || typeof g !== "object" || g.__fn) return null;

  // Template generator
  if (g.template != null && g.script == null && g.custom == null) {
    const tplVal = Array.isArray(g.template) ? g.template[0] : g.template;
    if (typeof tplVal !== "string") return null;
    const t = mapTemplate(tplVal);
    if (!t) return null;
    return { kind: "template", template: t };
  }

  // Script generator with pattern-matched postProcess.
  const pp = g.postProcess;
  const ppDesc = pp && typeof pp === "object" && pp.__post_process_kind
    ? pp.__post_process_kind
    : { kind: "none" };

  if (g.script != null && !g.__fn) {
    if (typeof g.script === "string") {
      return {
        kind: "script",
        input: { kind: "shell", script: g.script },
        split_on: typeof g.splitOn === "string" ? g.splitOn : "\n",
        post_process: ppDesc,
        timeout_ms: typeof g.scriptTimeout === "number" ? g.scriptTimeout : 5000,
      };
    }
    if (Array.isArray(g.script) && g.script.every((s: any) => typeof s === "string")) {
      return {
        kind: "script",
        input: { kind: "argv", argv: g.script },
        split_on: typeof g.splitOn === "string" ? g.splitOn : "\n",
        post_process: ppDesc,
        timeout_ms: typeof g.scriptTimeout === "number" ? g.scriptTimeout : 5000,
      };
    }
  }

  // Custom/complex postProcess closures: mark requiring JS — phase 6.
  return null;
}

function mapTemplate(t: string): string | null {
  switch (t) {
    case "filepaths":
      return "filepaths";
    case "folders":
      return "folders";
    case "history":
      return "history";
    case "help":
      return "help";
    default:
      return null;
  }
}

function clamp0_100(n: number): number {
  return Math.max(0, Math.min(100, Math.round(n)));
}

// ---------------------------------------------------------------------------
// Per-file extraction
// ---------------------------------------------------------------------------

function extractFile(project: Project, filePath: string, relName: string): void {
  stats.total++;
  const rel = relName;
  let sourceFile: SourceFile;
  try {
    sourceFile = project.addSourceFileAtPath(filePath);
  } catch (e) {
    stats.error++;
    return;
  }

  // Find the default export.
  const defaultExport = sourceFile.getDefaultExportSymbol();
  if (!defaultExport) {
    stats.error++;
    sourceFile.forget();
    return;
  }

  // Locate the object literal the default export resolves to.
  let specNode: ObjectLiteralExpression | null = null;
  for (const decl of defaultExport.getDeclarations()) {
    if (Node.isExportAssignment(decl)) {
      const expr = decl.getExpression();
      if (Node.isObjectLiteralExpression(expr)) {
        specNode = expr as ObjectLiteralExpression;
        break;
      }
      if (Node.isIdentifier(expr)) {
        const symbol = expr.getSymbol();
        if (symbol) {
          for (const d of symbol.getDeclarations()) {
            if (Node.isVariableDeclaration(d)) {
              const init = d.getInitializer();
              if (init && Node.isObjectLiteralExpression(init)) {
                specNode = init as ObjectLiteralExpression;
                break;
              }
              if (init && Node.isAsExpression(init)) {
                const inner = init.getExpression();
                if (Node.isObjectLiteralExpression(inner)) {
                  specNode = inner as ObjectLiteralExpression;
                  break;
                }
              }
            }
          }
        }
      }
    }
  }

  if (!specNode) {
    stats.js_only++;
    manifest.push({
      name: rel,
      file: `${rel}.ts`,
      kind: "js_only",
      has_functions: true,
    });
    sourceFile.forget();
    return;
  }

  const ctx: ExtractCtx = {
    has_functions: false,
    file: filePath,
    post_process_in_progress: false,
  };
  const raw = extractObject(specNode, ctx);
  sourceFile.forget();

  if (ctx.has_functions) {
    stats.function_skips++;
    stats.partial++;
    manifest.push({
      name: rel,
      file: `${rel}.ts`,
      kind: "partial",
      has_functions: true,
    });
    return;
  }

  const rust = toRustSubcommand(raw);
  if (!rust || !Array.isArray(rust.names) || rust.names.length === 0) {
    stats.error++;
    return;
  }

  // Write the JSON. Top-level specs in the ESSENTIALS whitelist go to
  // embed/ (compiled into the binary via include_dir). Everything else —
  // including nested aws/*, gcloud/*, and top-level specs not in the
  // whitelist — goes to extras/ (shipped separately or loaded at runtime
  // from ~/.local/share/insh-rs/extras).
  const baseName = path.basename(rel);
  const isEmbed = !rel.includes("/") && ESSENTIALS.has(baseName);
  const targetDir = isEmbed ? EMBED_DIR : EXTRAS_DIR;
  const outFile = path.join(targetDir, `${rel}.json`);
  fs.mkdirSync(path.dirname(outFile), { recursive: true });
  fs.writeFileSync(outFile, JSON.stringify(rust, null, 2) + "\n");
  stats.pure++;
  if (isEmbed) stats.essentials_extracted++;

  manifest.push({
    name: rel,
    file: `${rel}.ts`,
    kind: "pure",
    has_functions: false,
  });
}

function walkSpecs(dir: string, prefix: string = ""): Array<{ path: string; rel: string }> {
  const out: Array<{ path: string; rel: string }> = [];
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    // Skip hidden entries and @-scoped npm-style directories (helpers, not
    // specs) at the top level. Nested directories like aws/, gcloud/ ARE
    // recursed into.
    if (entry.name.startsWith(".") || entry.name.startsWith("@")) continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const sub = prefix ? `${prefix}/${entry.name}` : entry.name;
      out.push(...walkSpecs(full, sub));
    } else if (entry.isFile() && entry.name.endsWith(".ts")) {
      const base = entry.name.slice(0, -3);
      const rel = prefix ? `${prefix}/${base}` : base;
      out.push({ path: full, rel });
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function main(): void {
  if (!fs.existsSync(SRC)) {
    console.error(`SRC not found: ${SRC}`);
    process.exit(1);
  }
  // Wipe previous extractor output (both dirs) so stale files from an
  // earlier run don't survive when the whitelist changes.
  fs.rmSync(EMBED_DIR, { recursive: true, force: true });
  fs.rmSync(EXTRAS_DIR, { recursive: true, force: true });
  // Also remove the old essentials/ path from phase 3 if it still exists.
  fs.rmSync(path.join(OUT, "essentials"), { recursive: true, force: true });
  fs.mkdirSync(EMBED_DIR, { recursive: true });
  fs.mkdirSync(EXTRAS_DIR, { recursive: true });

  const project = new Project({
    compilerOptions: {
      target: 99, // ESNext
      module: 99, // ESNext
      strict: false,
      skipLibCheck: true,
      noEmit: true,
      allowJs: false,
    },
    useInMemoryFileSystem: false,
  });

  const entries = walkSpecs(SRC).sort((a, b) => a.rel.localeCompare(b.rel));

  for (const entry of entries) {
    try {
      extractFile(project, entry.path, entry.rel);
    } catch (e: any) {
      stats.error++;
      console.error(`error on ${entry.rel}: ${e?.message ?? e}`);
    }
  }

  // Write the manifest.
  fs.writeFileSync(
    path.join(OUT, "index.json"),
    JSON.stringify(
      {
        version: "upstream-unknown",
        stats,
        entries: manifest,
      },
      null,
      2
    ) + "\n"
  );

  console.log(JSON.stringify(stats, null, 2));
  console.log(`wrote ${stats.essentials_extracted} embed specs to ${EMBED_DIR}`);
  console.log(`wrote ${stats.pure - stats.essentials_extracted} extras specs to ${EXTRAS_DIR}`);
  console.log(`manifest: ${path.join(OUT, "index.json")}`);
}

main();
