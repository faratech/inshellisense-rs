/*!
 * insh-rs spec extractor.
 *
 * Walks @withfig/autocomplete/src/*.ts, parses each file with ts-morph,
 * locates the default-exported Fig.Spec object literal, and converts the
 * pure-data subset into JSON matching inshellisense-rs's Rust schema.
 *
 * Recognized static factories and generator idioms are converted directly.
 * Unsupported function-backed fields are dropped when the surrounding object
 * remains usable, so partial specs can still be emitted with their declarative
 * skeleton. No JS runtime is required.
 *
 * Output: one .json file per extracted spec into <out>/embed/ or <out>/extras/.
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
  kind: "pure" | "partial" | "js_only" | "error";
  has_functions: boolean;
}

const manifest: ManifestEntry[] = [];

// ---------------------------------------------------------------------------
// Value extraction
// ---------------------------------------------------------------------------

type ParamSubs = Map<string, any>;

type ExtractCtx = {
  has_functions: boolean;
  file: string;
  /// When true, the classifier is evaluating a function expression in the
  /// context of a generator's `postProcess` field — recognized shapes are
  /// converted to PostProcessKind variants instead of flagging the spec.
  post_process_in_progress: boolean;
  /// A3 factory evaluator: when extractValue encounters an Identifier whose
  /// name is in this map, substitute the mapped value. Used while walking
  /// a function body whose parameters are bound to call arguments.
  param_subs?: ParamSubs;
  /// Debug-only: captures the stack of impurity flip sites to help
  /// diagnose why specific specs end up partial.
  impure_trace?: string[];
};

function flipImpure(ctx: ExtractCtx, tag: string): void {
  ctx.has_functions = true;
  if (process.env.DEBUG_IMPURE_TRACE && ctx.impure_trace) {
    ctx.impure_trace.push(tag);
  }
}

/// Attempt to classify a function expression used as a postProcess
/// callback into a named PostProcessKind.  Returns the kind descriptor
/// on a hit, null on a miss (caller should fall back to marking the
/// containing spec as having functions).
/// A3: Static factory evaluator. Given a CallExpression, try to find the
/// callee's declaration, bind its parameters to the call arguments, and
/// recursively extract the return expression. Supports:
///   * top-level `const f = (a, b) => ({...})` then `f("x", "y")`
///   * top-level `function f(a, b) { return {...}; }` then `f("x", "y")`
///   * imported factories (via Identifier resolution through alias symbols)
///   * default-param substitution: `function f(a = {}) { ... }`
/// Returns the extracted value, or null on any failure (caller falls back
/// to marking the spec impure).
function tryEvaluateFactoryCall(call: Node, ctx: ExtractCtx): any | null {
  if (!Node.isCallExpression(call)) return null;
  const callee = call.getExpression();
  if (!Node.isIdentifier(callee)) return null;
  const sym = callee.getSymbol();
  if (!sym) return null;
  const target = sym.getAliasedSymbol?.() ?? sym;
  if (process.env.DEBUG_A3) {
    console.error(`A3: evaluating ${callee.getText()}(...) in ${ctx.file}`);
  }

  // Find the function/arrow declaration.
  let fn: ArrowFunction | FunctionExpression | null = null;
  let fnDecl: Node | null = null;
  for (const decl of target.getDeclarations()) {
    if (Node.isVariableDeclaration(decl)) {
      const init = decl.getInitializer();
      if (init && (Node.isArrowFunction(init) || Node.isFunctionExpression(init))) {
        fn = init as ArrowFunction | FunctionExpression;
        break;
      }
    }
    if (Node.isFunctionDeclaration(decl)) {
      fnDecl = decl;
      break;
    }
  }
  if (!fn && !fnDecl) {
    if (process.env.DEBUG_A3) console.error(`  no declaration found`);
    return null;
  }

  // Find the return expression.
  let returnExpr: Node | undefined;
  const getParams = () => (fn ? fn.getParameters() : (fnDecl as any).getParameters());
  const getBody = () => (fn ? fn.getBody() : (fnDecl as any).getBody?.());
  const body = getBody();
  if (body) {
    if (Node.isBlock(body)) {
      for (const stmt of body.getStatements()) {
        if (Node.isReturnStatement(stmt)) {
          returnExpr = stmt.getExpression();
          break;
        }
      }
    } else {
      returnExpr = body as Node;
    }
  }
  if (!returnExpr) {
    if (process.env.DEBUG_A3) console.error(`  no return expression`);
    return null;
  }
  while (returnExpr && Node.isParenthesizedExpression(returnExpr)) {
    returnExpr = returnExpr.getExpression();
  }
  if (!returnExpr) return null;

  // Bind parameters to arguments (with default-param support).
  const params = getParams();
  const args = call.getArguments();
  const subs: ParamSubs = new Map();
  for (let i = 0; i < params.length; i++) {
    const pname = params[i].getName?.() ?? params[i].getText();
    if (i < args.length) {
      const v = extractValue(args[i], ctx);
      subs.set(pname, v);
    } else {
      const initializer = params[i].getInitializer?.();
      if (initializer) {
        const v = extractValue(initializer, ctx);
        subs.set(pname, v);
      } else {
        subs.set(pname, undefined);
      }
    }
  }

  // Evaluate the return expression with the substitution map active.
  // Note: we accept partial extractions — if a non-tolerant-list inner
  // field flips has_functions, we still return what we have. Tolerant
  // list fields are already handling their own isolation; scalar impure
  // fields inside the body just become null and get dropped.
  const savedSubs = ctx.param_subs;
  ctx.param_subs = subs;
  const savedFns = ctx.has_functions;
  ctx.has_functions = false;
  const value = extractValue(returnExpr, ctx);
  // Impurity found *inside* the factory body must survive. Restoring
  // `savedFns` unconditionally discarded it, so a factory returning
  // `{ name: "x", description: dynamic() }` dropped `dynamic()` and was
  // still reported as a pure, full-fidelity extraction.
  const bodyHadFunctions = ctx.has_functions;
  ctx.has_functions = savedFns || bodyHadFunctions;
  ctx.param_subs = savedSubs;

  if (value == null) {
    if (process.env.DEBUG_A3) console.error(`  extraction returned null`);
    return null;
  }
  if (process.env.DEBUG_A3) console.error(`  OK`);
  return value;
}

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
  // A1: "foo" + "bar" — fold string/number concat at compile time.
  if (Node.isBinaryExpression(node)) {
    const op = node.getOperatorToken().getKind();
    if (op === SyntaxKind.PlusToken) {
      const left = extractValue(node.getLeft(), ctx);
      const right = extractValue(node.getRight(), ctx);
      if (typeof left === "string" && typeof right === "string") return left + right;
      if (typeof left === "number" && typeof right === "number") return left + right;
      if (typeof left === "string" && typeof right === "number") return left + String(right);
      if (typeof left === "number" && typeof right === "string") return String(left) + right;
    }
    flipImpure(ctx, `BinaryExpression@${node.getStartLineNumber()}`);
    return null;
  }
  if (Node.isTemplateExpression(node)) {
    // `\`foo ${bar} baz\`` — fold if every interpolation resolves to a
    // primitive value. Uses the same path as extractValue so parameter
    // substitutions from a surrounding factory evaluation apply.
    let out = node.getHead().getLiteralText();
    let ok = true;
    const savedFns = ctx.has_functions;
    for (const span of node.getTemplateSpans()) {
      const expr = span.getExpression();
      ctx.has_functions = false;
      const v = extractValue(expr, ctx);
      const spanImpure = ctx.has_functions;
      if (spanImpure || (typeof v !== "string" && typeof v !== "number" && typeof v !== "boolean")) {
        ok = false;
        break;
      }
      out += String(v);
      const tail = span.getLiteral().getLiteralText();
      out += tail;
    }
    ctx.has_functions = savedFns;
    if (ok) return out;
    // If we can't fold, don't taint the spec — return null and let the
    // caller (usually an isolation barrier or tolerant list field) drop
    // the property.
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
    const out: any[] = [];
    for (const el of arr.getElements()) {
      // A4: `[...commonOptions, { ... }]` — inline the spread if the
      // spread expression resolves to an array at compile time.
      if (Node.isSpreadElement(el)) {
        const resolved = extractValue(el.getExpression(), ctx);
        if (Array.isArray(resolved)) {
          out.push(...resolved);
          continue;
        }
        flipImpure(ctx, `SpreadElement@${el.getStartLineNumber()}`);
        continue;
      }
      out.push(extractValue(el, ctx));
    }
    return out;
  }
  if (Node.isObjectLiteralExpression(node)) {
    return extractObject(node, ctx);
  }
  if (Node.isIdentifier(node)) {
    // A3: parameter substitution — inside a factory body evaluation, an
    // identifier matching a bound parameter name returns the mapped value.
    if (ctx.param_subs) {
      const name = node.getText();
      if (ctx.param_subs.has(name)) {
        return ctx.param_subs.get(name);
      }
    }
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
            // If the initializer is itself a factory call, evaluate it.
            if (Node.isCallExpression(init)) {
              const v = tryEvaluateFactoryCall(init, ctx);
              if (v !== null) return v;
            }
            return extractValue(init, ctx);
          }
        }
      }
    }
    flipImpure(ctx, `Identifier-unresolved(${node.getText()})@${node.getStartLineNumber()}`);
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
    flipImpure(ctx, `fn-unclassified@${node.getStartLineNumber()}`);
    return { __fn: true };
  }
  if (Node.isMethodDeclaration(node)) {
    flipImpure(ctx, `MethodDecl@${node.getStartLineNumber()}`);
    return { __fn: true };
  }
  if (Node.isAsExpression(node) || Node.isParenthesizedExpression(node)) {
    return extractValue(node.getExpression(), ctx);
  }
  if (Node.isSpreadElement(node)) {
    flipImpure(ctx, `SpreadElement-top@${node.getStartLineNumber()}`);
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
    flipImpure(ctx, `PropertyAccessExpression@${node.getStartLineNumber()}`);
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
    // A3: try evaluating the call as a pure factory that returns a literal.
    const factoryResult = tryEvaluateFactoryCall(node, ctx);
    if (factoryResult !== null) return factoryResult;
    const calleeText = callee ? callee.getText().slice(0, 40) : "?";
    flipImpure(ctx, `CallExpression(${calleeText})@${node.getStartLineNumber()}`);
    return null;
  }

  // Unknown node shape — conservative: mark impure.
  flipImpure(ctx, `UnknownNode(${node.getKindName()})@${node.getStartLineNumber()}`);
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

/// Fields where the WHOLE field can be dropped silently if it contains
/// functions, without flagging the entire spec as partial. Used for
/// `generateSpec`/`getVersionCommand`/`onlyShowAt` and similar runtime
/// hooks that have no static representation but whose absence does not
/// invalidate the spec. We replace them with null in the output.
const TOLERANT_SCALAR_FIELDS = new Set([
  "generateSpec",
  "getVersionCommand",
  // `loadSpec` as a function (not a string) is JS-only — drop silently.
  // If it's a string, the existing toRustSubcommand path handles it.
  "loadSpec",
  // Various rare runtime hooks.
  "isCommand",
  "filterTerm",
  "getQueryTerm",
  "shouldRedraw",
]);

/// Extract a value with an "isolation barrier" — if anything in its
/// subtree sets has_functions, the flag is reset and the function returns
/// null instead of polluting the outer ctx.
function extractValueIsolated(node: Node | undefined, ctx: ExtractCtx): any {
  const saved = ctx.has_functions;
  const savedLen = ctx.impure_trace?.length ?? 0;
  ctx.has_functions = false;
  const v = extractValue(node, ctx);
  const wasImpure = ctx.has_functions;
  ctx.has_functions = saved;
  // Pop any trace entries added during isolation — the outer spec's
  // trace should only show flips that actually propagated.
  if (ctx.impure_trace && wasImpure) {
    ctx.impure_trace.length = savedLen;
  }
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
      } else if (TOLERANT_SCALAR_FIELDS.has(name)) {
        // Phase 6.2: drop these fields entirely if impure, but don't
        // taint the surrounding spec. They have no static representation
        // (they're runtime hooks) and their absence doesn't break
        // anything — Rust just won't run them.
        const v = extractValueIsolated(pa.getInitializer(), ctx);
        if (v !== null && v !== undefined) {
          out[name] = v;
        }
      } else {
        const value = extractValue(pa.getInitializer(), ctx);
        if (value !== undefined) {
          out[name] = value;
        }
      }

      if (isPostProcess) ctx.post_process_in_progress = savedFlag;
    } else if (Node.isShorthandPropertyAssignment(prop)) {
      // `{ name }` is sugar for `{ name: name }` — resolve the
      // identifier to its value. ts-morph: getNameNode().getSymbol()
      // returns the property's own symbol, not the resolved variable;
      // we need getValueSymbol() to follow to the actual definition.
      const propName = prop.getName();
      // Param substitution: if we're inside a factory body and the
      // shorthand name matches a bound parameter, return the value.
      if (ctx.param_subs && ctx.param_subs.has(propName)) {
        const v = ctx.param_subs.get(propName);
        if (v !== undefined && v !== null) {
          out[propName] = v;
        }
        continue;
      }
      const valueSym = prop.getValueSymbol();
      if (valueSym) {
        const target = valueSym.getAliasedSymbol?.() ?? valueSym;
        let resolved: any = null;
        for (const decl of target.getDeclarations()) {
          if (Node.isVariableDeclaration(decl)) {
            const init = decl.getInitializer();
            if (init) {
              resolved = extractValue(init, ctx);
              break;
            }
          }
        }
        if (resolved !== null && resolved !== undefined) {
          out[propName] = resolved;
        } else {
          flipImpure(ctx, `Shorthand-unresolved(${propName})@${prop.getStartLineNumber()}`);
        }
      } else {
        flipImpure(ctx, `Shorthand-no-value-sym(${propName})@${prop.getStartLineNumber()}`);
      }
    } else if (Node.isSpreadAssignment(prop)) {
      // A4: `{ ...commonFields, name: "foo" }` — resolve the spread's
      // source and Object.assign its keys into the current object.
      const resolved = extractValue(prop.getExpression(), ctx);
      if (resolved && typeof resolved === "object" && !Array.isArray(resolved)) {
        Object.assign(out, resolved);
      } else {
        flipImpure(ctx, `SpreadAssignment@${prop.getStartLineNumber()}`);
      }
    } else if (Node.isMethodDeclaration(prop)) {
      // Method shorthand: `postProcess(out) { return ...; }` or
      // `generateSpec(tokens, exec) { ... }`. Duck-type the node to the
      // ArrowFunction/FunctionExpression shape for the classifier.
      const name = prop.getName();
      const isPostProcess = name === "postProcess";
      let classified: any = null;
      if (isPostProcess) {
        classified =
          classifyPostProcess(prop as any) ??
          classifyJsonParsePostProcess(prop as any);
      }
      if (classified) {
        out[name] = { __post_process_kind: classified };
      } else if (TOLERANT_SCALAR_FIELDS.has(name)) {
        // Runtime hook (generateSpec/loadSpec/etc.) in method-shorthand
        // form. Drop silently like we do for the arrow/function form.
      } else {
        // Method shorthand we can't classify → impure for this field.
        // Tolerant-list isolation in the parent generator will drop
        // the surrounding object.
        flipImpure(ctx, `MethodShorthand(${name})@${prop.getStartLineNumber()}`);
      }
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
// Phase 6.2: post-extraction idiom injections
// ---------------------------------------------------------------------------
//
// For specs whose generateSpec/custom/postProcess functions match known
// idioms that we re-implement in Rust, inject the corresponding new
// Generator variants into the extracted Rust shape after conversion.
// This is a deliberate "by spec name" hardcoding — much safer than
// trying to AST-pattern-match the bodies of arbitrarily-shaped factory
// functions.

type Injection = (rust: any) => void;

const INJECTIONS: Record<string, Injection> = {
  // pnpm/yarn/bun all share the "list installed CLI tools as
  // subcommands" idiom. Add a top-level args generator that reads
  // package.json scripts (so `pnpm <TAB>` shows scripts) AND a
  // synthetic loadable subcommand for each known node CLI found in
  // package.json deps.
  pnpm: injectPackageJsonRunner,
  yarn: injectPackageJsonRunner,
  bun: injectPackageJsonRunner,

  // python detects Django via `manage.py` containing "django".
  python: (rust) => {
    addFileExistsSubcommand(rust, {
      path: "manage.py",
      content_contains: "django",
      subcommand: {
        names: ["manage.py"],
        description: "Django manage.py — load django-admin spec",
      },
    });
  },
  python3: (rust) => {
    addFileExistsSubcommand(rust, {
      path: "manage.py",
      content_contains: "django",
      subcommand: {
        names: ["manage.py"],
        description: "Django manage.py — load django-admin spec",
      },
    });
  },

  // node detects AdonisJS via the marker file. The upstream spec
  // returns a 400-line embedded spec for ace; we provide a placeholder
  // subcommand that loads adonis if present.
  node: (rust) => {
    addFileExistsSubcommand(rust, {
      path: "ace",
      subcommand: {
        names: ["ace"],
        description: "AdonisJS ace command (detected via ./ace)",
      },
    });
  },

  // php detects laravel/symfony/please via marker files.
  php: (rust) => {
    addFileExistsSubcommand(rust, {
      path: "artisan",
      subcommand: {
        names: ["artisan"],
        description: "Laravel artisan",
      },
    });
    addFileExistsSubcommand(rust, {
      path: "please",
      subcommand: {
        names: ["please"],
        description: "Laravel please",
      },
    });
    addFileExistsSubcommand(rust, {
      path: "bin/console",
      subcommand: {
        names: ["bin/console"],
        description: "Symfony bin/console",
      },
    });
  },
};

function injectPackageJsonRunner(rust: any): void {
  // Top-level args: read package.json scripts.
  if (!Array.isArray(rust.args)) rust.args = [];
  const argEntry: any = {
    name: "script",
    description: "package.json script",
    is_optional: true,
    is_variadic: true,
    generators: [
      { kind: "project_file", reader: "package_json_scripts" },
    ],
  };
  // Avoid double-injecting if a previous run added it.
  const already = rust.args.some(
    (a: any) =>
      Array.isArray(a.generators) &&
      a.generators.some(
        (g: any) =>
          g.kind === "project_file" && g.reader === "package_json_scripts"
      )
  );
  if (!already) {
    rust.args.unshift(argEntry);
  }

  // Add a synthetic top-level args generator for node CLIs.
  if (!already) {
    argEntry.generators.push({
      kind: "project_file",
      reader: "package_json_node_clis",
    });
    argEntry.generators.push({
      kind: "project_file",
      reader: "node_modules_binaries",
    });
  }
}

function addFileExistsSubcommand(
  rust: any,
  cfg: { path: string; content_contains?: string; subcommand: any }
): void {
  if (!Array.isArray(rust.args)) rust.args = [];
  // Mount the FileExistsThen as a top-level arg generator so that the
  // bare command (e.g. `python <TAB>`) surfaces the marker subcommand.
  let argEntry = rust.args.find((a: any) => a && Array.isArray(a.generators));
  if (!argEntry) {
    argEntry = {
      name: "context",
      is_optional: true,
      generators: [],
    };
    rust.args.unshift(argEntry);
  }
  const gen: any = {
    kind: "file_exists_then",
    path: cfg.path,
    subcommand: cfg.subcommand,
  };
  if (cfg.content_contains) gen.content_contains = cfg.content_contains;
  // Avoid double-injecting on re-runs.
  const already = argEntry.generators.some(
    (g: any) => g.kind === "file_exists_then" && g.path === cfg.path
  );
  if (!already) argEntry.generators.push(gen);
}

function applyPostExtractInjection(rust: any): void {
  if (!rust || !Array.isArray(rust.names)) return;
  const primary = rust.names[0];
  const inject = INJECTIONS[primary];
  if (inject) {
    inject(rust);
    if (process.env.DEBUG_INJECT) {
      console.error(`injected idiom for ${primary}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Phase 6.1: createVersionedSpec handler
// ---------------------------------------------------------------------------

/// Pick the highest semver version from a list of "X.Y.Z" strings.
/// Falls back to lexicographic comparison if a string isn't valid semver.
function pickHighestVersion(versions: string[]): string | null {
  if (versions.length === 0) return null;
  const parsed = versions.map((v) => {
    const m = v.match(/^(\d+)\.(\d+)\.(\d+)/);
    if (m) {
      return {
        v,
        nums: [parseInt(m[1], 10), parseInt(m[2], 10), parseInt(m[3], 10)],
      };
    }
    return { v, nums: null };
  });
  // If all parsed: numeric sort.
  if (parsed.every((p) => p.nums !== null)) {
    parsed.sort((a, b) => {
      for (let i = 0; i < 3; i++) {
        if (a.nums![i] !== b.nums![i]) return a.nums![i] - b.nums![i];
      }
      return 0;
    });
  } else {
    parsed.sort((a, b) => a.v.localeCompare(b.v));
  }
  return parsed[parsed.length - 1].v;
}

/// Detect `createVersionedSpec(name, versions)` and recursively extract
/// from the highest-version sibling file. Returns true if handled.
function tryHandleCreateVersionedSpec(
  project: Project,
  call: Node,
  filePath: string,
  relName: string
): boolean {
  if (!Node.isCallExpression(call)) return false;
  const callee = call.getExpression();
  if (!Node.isIdentifier(callee)) return false;
  if (callee.getText() !== "createVersionedSpec") return false;

  const args = call.getArguments();
  if (args.length < 2) return false;

  // Both args must fold to literals.
  const ctxTmp: ExtractCtx = {
    has_functions: false,
    file: filePath,
    post_process_in_progress: false,
  };
  const nameArg = extractValue(args[0], ctxTmp);
  const versionsArg = extractValue(args[1], ctxTmp);
  if (typeof nameArg !== "string") return false;
  if (!Array.isArray(versionsArg)) return false;
  const versions: string[] = versionsArg.filter((v) => typeof v === "string");
  if (versions.length === 0) return false;

  const latest = pickHighestVersion(versions);
  if (!latest) return false;

  // Sibling path: e.g. /tmp/withfig-autocomplete/src/heroku/8.6.0.ts
  // The current file is e.g. /tmp/withfig-autocomplete/src/heroku/index.ts
  const targetDir = path.dirname(filePath);
  const targetPath = path.join(targetDir, `${latest}.ts`);
  if (!fs.existsSync(targetPath)) {
    if (process.env.DEBUG_A3) {
      console.error(
        `createVersionedSpec target missing: ${targetPath} from ${filePath}`
      );
    }
    return false;
  }

  // The original relName for an index file is e.g. "heroku/index". We
  // want the emitted spec keyed under the parent dir name ("heroku") so
  // that `is complete "heroku ..."` finds it via the top-level lookup.
  // If the relName ends with "/index", strip the suffix.
  let emitName = relName;
  if (emitName.endsWith("/index")) {
    emitName = emitName.slice(0, -"/index".length);
  }

  if (process.env.DEBUG_VERSIONED) {
    console.error(
      `createVersionedSpec ${nameArg} → ${latest} (${path.basename(targetPath)}) for ${relName} → emit as ${emitName}`
    );
  }
  extractFileAs(project, targetPath, emitName);
  return true;
}

// ---------------------------------------------------------------------------
// Per-file extraction
// ---------------------------------------------------------------------------

function extractFile(project: Project, filePath: string, relName: string): void {
  stats.total++;
  extractFileAs(project, filePath, relName);
}

function extractFileAs(project: Project, filePath: string, relName: string): void {
  const rel = relName;
  let sourceFile: SourceFile;
  try {
    sourceFile = project.addSourceFileAtPath(filePath);
  } catch (e) {
    stats.error++;
    return;
  }

  // A2: `export { default } from "./git"` — follow the re-export.
  // ts-morph emits these as ExportDeclaration nodes, not as the default
  // export symbol. Detect and recursively extract from the target file.
  for (const decl of sourceFile.getExportDeclarations()) {
    const spec = decl.getModuleSpecifierValue();
    if (!spec) continue;
    const hasDefault = decl
      .getNamedExports()
      .some((e) => e.getName() === "default");
    if (!hasDefault) continue;
    const targetRel = `${spec}.ts`;
    const targetPath = path.resolve(path.dirname(filePath), targetRel);
    if (!fs.existsSync(targetPath)) {
      sourceFile.forget();
      stats.error++;
      console.error(`re-export target not found: ${targetPath} from ${filePath}`);
      return;
    }
    // Recursively extract from the target, then rewrite the output's
    // primary name to be this file's basename.
    sourceFile.forget();
    extractFileAs(project, targetPath, relName);
    return;
  }

  // Find the default export.
  const defaultExport = sourceFile.getDefaultExportSymbol();
  if (!defaultExport) {
    stats.error++;
    console.error(`no default export in ${filePath}`);
    sourceFile.forget();
    return;
  }

  // Locate the object literal the default export resolves to.
  let specNode: ObjectLiteralExpression | null = null;
  // A3 path: `export default factory()` — evaluate the factory and
  // emit the result as raw JSON without going through specNode.
  let rawFromFactory: any = null;
  for (const decl of defaultExport.getDeclarations()) {
    if (Node.isExportAssignment(decl)) {
      const expr = decl.getExpression();
      if (Node.isObjectLiteralExpression(expr)) {
        specNode = expr as ObjectLiteralExpression;
        break;
      }
      if (Node.isCallExpression(expr)) {
        // Phase 6.1: `export default createVersionedSpec(name, versions)`.
        // Detect this BEFORE the generic factory evaluator because the
        // factory body returns a closure (not a Fig.Spec), so the
        // generic path would fail. We instead pick the highest semver
        // version, compute the sibling spec path, and recursively
        // extract that file under the index's relative name.
        if (tryHandleCreateVersionedSpec(project, expr, filePath, relName)) {
          sourceFile.forget();
          return;
        }
        // A3: `export default completionSpec()`.
        const ctxTmp: ExtractCtx = {
          has_functions: false,
          file: filePath,
          post_process_in_progress: false,
        };
        const v = tryEvaluateFactoryCall(expr, ctxTmp);
        if (v && !ctxTmp.has_functions) {
          rawFromFactory = v;
          break;
        }
      }
      if (Node.isIdentifier(expr)) {
        const symbol = expr.getSymbol();
        if (symbol) {
          // Phase 6.1b: `import spec from "./other"; export default spec`
          // — follow the default import to its source file and extract
          // from there with the current relName.
          for (const d of symbol.getDeclarations()) {
            if (Node.isImportClause(d)) {
              const importDecl = d.getParent();
              if (importDecl && Node.isImportDeclaration(importDecl)) {
                const moduleSpec = importDecl.getModuleSpecifierValue();
                if (moduleSpec && (moduleSpec.startsWith("./") || moduleSpec.startsWith("../"))) {
                  const targetPath = path.resolve(
                    path.dirname(filePath),
                    `${moduleSpec}.ts`
                  );
                  if (fs.existsSync(targetPath)) {
                    sourceFile.forget();
                    extractFileAs(project, targetPath, relName);
                    return;
                  }
                }
              }
            }
          }
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
              // A3: `const completionSpec = generateXSpec("name", "Display")`
              if (init && Node.isCallExpression(init)) {
                const ctxTmp: ExtractCtx = {
                  has_functions: false,
                  file: filePath,
                  post_process_in_progress: false,
                };
                const v = tryEvaluateFactoryCall(init, ctxTmp);
                if (v && !ctxTmp.has_functions) {
                  rawFromFactory = v;
                  break;
                }
              }
            }
          }
        }
      }
    }
  }

  if (!specNode && rawFromFactory === null) {
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
    impure_trace: process.env.DEBUG_IMPURE_TRACE ? [] : undefined,
  };
  const raw = rawFromFactory !== null ? rawFromFactory : extractObject(specNode!, ctx);
  sourceFile.forget();
  if (process.env.DEBUG_IMPURE_TRACE && ctx.impure_trace && ctx.impure_trace.length > 0) {
    console.error(`${rel} impure sites (has_functions=${ctx.has_functions}, ${ctx.impure_trace.length} flips):`);
    for (const t of ctx.impure_trace.slice(0, 15)) {
      console.error(`  ${t}`);
    }
  }

  // Phase 6.2d: even when has_functions is true, the static fields we
  // already extracted are usable. Emit anyway so the spec loads with
  // partial quality (top-level subcommands + options work; dynamic
  // generators that couldn't resolve are simply absent). This pushes
  // the corpus to literal 100%.
  const wasPartial = ctx.has_functions;

  // Classify only after conversion succeeds. Counting a spec as `pure`
  // up-front meant one that failed to convert incremented `pure` *and*
  // `error`, wrote no JSON, and still landed a `kind: "pure"` manifest
  // entry — so the pure/total ratio read 100% while the spec vanished.
  const rust = toRustSubcommand(raw);
  if (!rust || !Array.isArray(rust.names) || rust.names.length === 0) {
    stats.error++;
    if (wasPartial) stats.function_skips++;
    if (process.env.DEBUG_EMIT) {
      console.error(
        `empty/missing names after conversion: ${filePath} wasPartial=${wasPartial} raw_keys=${
          raw && typeof raw === "object" ? Object.keys(raw).join(",") : typeof raw
        }`
      );
    }
    manifest.push({
      name: rel,
      file: `${rel}.ts`,
      kind: "error",
      has_functions: wasPartial,
    });
    return;
  }

  if (wasPartial) {
    stats.function_skips++;
    stats.partial++;
  } else {
    stats.pure++;
  }
  if (process.env.DEBUG_EMIT && wasPartial) {
    console.error(`emit-partial: ${rel}`);
  }

  // Phase 6.2: post-extraction injections for known idioms. Add
  // ProjectFile / FileExistsThen generators to specs whose generateSpec
  // we can replicate in Rust at completion time.
  applyPostExtractInjection(rust);
  // A2: when we recurse from a re-export, rewrite the primary name so
  // the emitted spec is keyed under the file that aliased it (e.g.
  // hub.json carries `names: ["hub"]`, not `["git"]`).
  if (rust.names[0] !== path.basename(rel)) {
    rust.names = [path.basename(rel), ...rust.names.filter((n: string) => n !== path.basename(rel))];
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
  if (isEmbed) stats.essentials_extracted++;

  manifest.push({
    name: rel,
    file: `${rel}.ts`,
    kind: wasPartial ? "partial" : "pure",
    has_functions: wasPartial,
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
      // Skip TypeScript declaration files (.d.ts) — they're type
      // definitions, not specs. Skip shared helper files (shared.ts)
      // which are imported by sibling specs but have no default export.
      // Skip known non-spec utility files from the upstream corpus.
      if (entry.name.endsWith(".d.ts")) continue;
      if (entry.name === "shared.ts") continue;
      if (entry.name === "generators.ts") continue;
      // Phase 6.1: aws/regions.ts is `export default <string[]>` — a data
      // helper imported by other specs, not a Fig.Spec. Filter it out so
      // it doesn't show up as a non-spec error in the corpus.
      if (prefix === "aws" && entry.name === "regions.ts") continue;
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

  // Phase 6.2c: eager-load every spec file into the ts-morph project so
  // cross-file symbol resolution works (e.g. pnpm imports
  // dependenciesGenerator from ./yarn). Without this the imported
  // symbol resolves to an ImportSpecifier we can't walk into.
  for (const entry of entries) {
    try {
      project.addSourceFileAtPathIfExists(entry.path);
    } catch (e) {
      // ignore — extractFile will report it
    }
  }

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
