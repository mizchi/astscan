// Values carry a possible witness per origin and, for simple objects, a heap
// identity. A summary uses symbolic origins for parameters and their fields.
const clean = { flows: new Map() };
const evidenceLimit = 128;
const truncated = { kind: "truncated", message: "Earlier and later flow steps are shown.", locations: [] };

function value(key, path = [], ref) {
  return { flows: new Map([[key, path]]), ref };
}

function join(a, b) {
  if (a === b) return a;
  const flows = new Map(a.flows);
  for (const [origin, path] of b.flows) {
    if (!flows.has(origin) || path.length < flows.get(origin).length) flows.set(origin, path);
  }
  return { flows, ref: a.ref === b.ref ? a.ref : undefined };
}

function concatPath(...segments) {
  const path = segments.flat();
  return path.length <= evidenceLimit ? path : [...path.slice(0, 64), truncated, ...path.slice(-63)];
}

function append(input, step) {
  return { ...input, flows: new Map([...input.flows].map(([origin, path]) => [origin, concatPath(path, [step])])) };
}

function parameter(index) {
  const key = `arg:${index}`;
  return value(key, [], key);
}

function initialField(key) {
  return key.startsWith("arg:") ? value(key) : clean;
}

function fieldValue(state, key) {
  return state.fields.get(key) ?? initialField(key);
}

function fork(state) {
  return { locals: new Map(state.locals), fields: new Map(state.fields) };
}

function merge(a, b) {
  if (!a || !b) return a ?? b;
  const locals = new Map(a.locals);
  for (const [id, val] of b.locals) locals.set(id, join(a.locals.get(id) ?? clean, val));
  for (const [id, val] of a.locals) if (!b.locals.has(id)) locals.set(id, join(val, clean));
  const fields = new Map();
  for (const key of new Set([...a.fields.keys(), ...b.fields.keys()])) fields.set(key, join(fieldValue(a, key), fieldValue(b, key)));
  return { locals, fields };
}

function sameBindings(a, b) {
  if (a.size !== b.size) return false;
  for (const [key, left] of a) {
    const right = b.get(key);
    if (!right || left.ref !== right.ref || left.flows.size !== right.flows.size) return false;
    for (const origin of left.flows.keys()) if (!right.flows.has(origin)) return false;
  }
  return true;
}

function sameState(a, b) {
  // Witness lengths can grow on a loop edge without changing reachability.
  return sameBindings(a.locals, b.locals) && sameBindings(a.fields, b.fields);
}

function keyParts(key) {
  const dot = key.indexOf(".");
  return dot < 0 ? [Number(key.slice(4)), undefined] : [Number(key.slice(4, dot)), key.slice(dot + 1)];
}

export function auditFunction(facts, root, rule, captures, limits) {
  const { ast, SyntaxKind: K, NodeFlags } = facts;
  const summaries = new Map();
  const active = new Set();
  const issues = new Map();
  const modelCache = new Map();
  const arrayLengths = new Map();
  let remaining = limits.steps;
  let loopDepth = 0;

  const isAsync = (fn) => fn.modifiers?.some((modifier) => modifier.kind === K.AsyncKeyword);

  function unparenthesized(node) {
    while (ast.isParenthesizedExpression(node)) node = node.expression;
    return node;
  }

  function issue(node, code, message) {
    const key = `${code}:${node.id}`;
    if (!issues.has(key)) issues.set(key, { code, message, location: facts.location(node) });
  }

  function step(node, kind, message) {
    return { kind, message, locations: [facts.location(node)] };
  }

  function spent(node, depth) {
    if (--remaining >= 0 && depth <= limits.nesting) return false;
    issue(node, "analysis-limit", "The analysis step or syntax nesting limit was reached.");
    return true;
  }

  function model(binding) {
    if (!binding) return;
    if (!modelCache.has(binding.id)) modelCache.set(binding.id, rule.models.find((one) => facts.declarationMatches(binding, one)));
    return modelCache.get(binding.id);
  }

  function callee(call) {
    return call.questionDotToken || call.expression.questionDotToken ? undefined : facts.callee(call.expression);
  }

  function allocate(node, kind) {
    if (loopDepth) issue(node, "loop-allocation", "Repeated allocation at this site can create distinct object identities.");
    return `${kind}:${node.id}`;
  }

  function arrayOf(input) {
    const length = arrayLengths.get(input.ref);
    if (length !== undefined) return { ref: input.ref, length };
  }

  function arrayAccess(node, state, depth) {
    const array = arrayOf(expression(node.expression, state, depth + 1));
    const indexNode = node.argumentExpression;
    expression(indexNode, state, depth + 1);
    if (!array || !indexNode || node.questionDotToken || state.stopped) return;
    if (!ast.isNumericLiteral(indexNode)) {
      if (facts.typeIs(indexNode, "number") === true) return array;
      return;
    }
    const index = Number(indexNode.text);
    if (Number.isSafeInteger(index) && index >= 0) return { ...array, index };
  }

  function slot(node, state, depth) {
    if (ast.isIdentifier(node)) {
      const id = facts.symbol(node)?.id;
      if (id !== undefined && state.locals.has(id)) return { map: state.locals, key: id };
    } else if (ast.isPropertyAccessExpression(node) && !node.questionDotToken) {
      const ref = expression(node.expression, state, depth + 1).ref;
      if (ref !== undefined && !arrayLengths.has(ref)) return { map: state.fields, key: `${ref}.${node.name.text}` };
    } else if (ast.isElementAccessExpression(node)) {
      const site = arrayAccess(node, state, depth);
      if (site && site.index !== undefined && site.index < site.length) return { map: state.fields, key: `${site.ref}.${site.index}` };
    }
    return undefined;
  }

  function write(target, input, node, kind = "assignment") {
    target.map.set(target.key, input.flows.size ? append(input, step(node, kind, kind === "assignment" ? "Assigned here." : "Value propagated here.")) : input);
  }

  function dependent(input, args, state, site) {
    let result = clean;
    for (const [key, suffix] of input.flows) {
      if (!key.startsWith("arg:")) {
        result = join(result, value(key, suffix));
        continue;
      }
      const [index, field] = keyParts(key);
      let actual = args[index] ?? clean;
      if (field !== undefined) {
        if (actual.ref === undefined) {
          issue(site, "unresolved-field", "Could not resolve the argument object for a field read.");
          continue;
        }
        actual = fieldValue(state, `${actual.ref}.${field}`);
      }
      const callStep = step(site, "argument", "Passed through this call.");
      for (const [origin, prefix] of actual.flows) result = join(result, value(origin, concatPath(prefix, [callStep], suffix)));
    }
    const ref = input.ref?.startsWith("arg:") ? args[keyParts(input.ref)[0]]?.ref : undefined;
    return { ...result, ref };
  }

  function observe(events, event, observed) {
    if (!observed.flows.size) return;
    const prior = events.get(event.site.id);
    const combined = prior ? join(prior.value, observed) : observed;
    events.set(event.site.id, { ...event, value: combined });
  }

  const eventStack = [];
  const callStack = [];

  function arrayCall(node, method, state, depth) {
    const array = arrayOf(expression(node.expression.expression, state, depth + 1));
    if (state.stopped) return clean;
    const arg = node.arguments.length === 1 && unparenthesized(node.arguments[0]);
    const callback = arg && (ast.isArrowFunction(arg) || ast.isFunctionExpression(arg)
      ? arg
      : ast.isIdentifier(arg) && facts.definition(facts.callee(arg)));
    if (!array || !callback?.body || callback.parameters.length > 1 || isAsync(callback) || callback.asteriskToken) {
      issue(node, "unsupported-array-call", "Expected a local array and one synchronous callback accepting at most one parameter.");
      return clean;
    }

    const ref = method === "map" ? allocate(node, "array") : undefined;
    for (let index = 0; index < array.length; index++) {
      if (index === limits.loops) {
        issue(node, "loop-limit", "The array method reached its iteration budget.");
        return clean;
      }
      if (spent(node, depth)) return clean;
      const element = fieldValue(state, `${array.ref}.${index}`);
      const result = applyCall(node, state, depth, callback, [element]);
      if (state.stopped) return clean;
      if (ref) write({ map: state.fields, key: `${ref}.${index}` }, result, node, "element");
    }
    if (!ref) return clean;
    arrayLengths.set(ref, array.length);
    return { ...clean, ref };
  }

  function callExpression(node, state, depth, awaited = false) {
    const binding = callee(node);
    const modeled = model(binding);
    if (!modeled && ast.isPropertyAccessExpression(node.expression)) {
      const method = facts.standardArrayMethod(binding);
      if (method) return arrayCall(node, method, state, depth);
    }
    const args = [];
    for (const arg of node.arguments) {
      if (ast.isSpreadElement(arg)) {
        issue(arg, "unsupported-spread", "Spread call arguments are outside the argument model.");
        return clean;
      }
      args.push(expression(arg, state, depth + 1));
      if (state.stopped) return clean;
    }
    if (modeled) {
      const arg = node.arguments[modeled.argument];
      const input = args[modeled.argument] ?? clean;
      switch (modeled.kind) {
        case "source": return value(`source:${node.id}`, [step(node, "source", "External input originates here.")]);
        case "sink": {
          if (arg) observe(eventStack.at(-1), { site: arg, call: node, model: modeled }, input);
          return clean;
        }
        case "sanitize-return":
        case "guard": return clean;
        case "propagate-return": return append(input, step(node, "propagate", "Value returned by a modeled call."));
        case "sanitize-field":
        case "propagate-field": {
          const ref = modeled.kind === "sanitize-field" ? input.ref : args[modeled.target]?.ref;
          if (ref === undefined) issue(node, "unresolved-effect", "Could not resolve the argument object for the modeled write.");
          else write({ map: state.fields, key: `${ref}.${modeled.field}` }, modeled.kind === "sanitize-field" ? clean : input, node, "effect");
          return clean;
        }
      }
    }
    const definition = binding && facts.definition(binding);
    if (!definition) {
      issue(node, "unresolved-call", "This call has no trusted model or supported definition.");
      return clean;
    }
    if (isAsync(definition) && !awaited) {
      issue(node, "unawaited-call", "The execution and effects of this async call were not joined by await.");
      return clean;
    }
    return applyCall(node, state, depth, definition, args);
  }

  function applyCall(node, state, depth, definition, args) {
    const summary = summarize(definition, callStack.at(-1) + 1, depth + 1);
    if (!summary) return clean;
    // Reads and return values are computed from the pre-call state. Writes
    // from different paths have already been joined in the callee summary.
    const effects = new Map();
    for (const [key, effect] of summary.effects) {
      const [index, field] = keyParts(key);
      const ref = args[index]?.ref;
      if (ref === undefined) issue(node, "unresolved-effect", "Could not resolve an object written by the callee.");
      else {
        if (args.some((other, position) => position !== index && other.ref === ref)) {
          issue(node, "aliased-parameters", "Two parameters can refer to the same object written by the callee.");
        }
        const target = `${ref}.${field}`;
        const value = dependent(effect, args, state, node);
        effects.set(target, effects.has(target) ? join(effects.get(target), value) : value);
      }
    }
    for (const event of summary.events.values()) {
      if (spent(node, depth)) break;
      observe(eventStack.at(-1), event, dependent(event.value, args, state, node));
    }
    if (!summary.returnsNormally) {
      state.stopped = true;
      return clean;
    }
    const returned = dependent(summary.returned, args, state, node);
    for (const [key, effect] of effects) write({ map: state.fields, key }, effect, node, "effect");
    return returned.flows.size ? append(returned, step(node, "return", "Returned to this call site.")) : returned;
  }

  function expression(node, state, depth) {
    if (!node || state.stopped || spent(node, depth)) return clean;
    if (ast.isElementAccessExpression(node)) {
      const site = arrayAccess(node, state, depth);
      if (!site) {
        if (!state.stopped) issue(node, "unresolved-index", "Expected a local array and a numeric index.");
        return clean;
      }
      if (site.index !== undefined) return site.index < site.length ? fieldValue(state, `${site.ref}.${site.index}`) : clean;
      let result = clean;
      for (let index = 0; index < site.length && !spent(node, depth); index++) {
        result = join(result, fieldValue(state, `${site.ref}.${index}`));
      }
      return result;
    }
    if (ast.isIdentifier(node) || ast.isPropertyAccessExpression(node)) {
      const target = slot(node, state, depth);
      if (target) return target.map.get(target.key) ?? initialField(target.key);
      issue(node, "unresolved-value", "The value is outside the local binding or field model.");
      return clean;
    }
    if (ast.isCallExpression(node)) return callExpression(node, state, depth);
    if (ast.isAwaitExpression(node)) {
      const operand = unparenthesized(node.expression);
      if (!ast.isCallExpression(operand)) {
        expression(operand, state, depth + 1);
        issue(node, "unresolved-await", "Only direct calls are modeled as awaited operations.");
        return clean;
      }
      const input = callExpression(operand, state, depth + 1, true);
      return input.flows.size ? append(input, step(node, "await", "Value received through await.")) : input;
    }
    if (ast.isParenthesizedExpression(node) || ast.isAsExpression(node) || ast.isNonNullExpression(node) || ast.isSatisfiesExpression(node)) {
      return expression(node.expression, state, depth + 1);
    }
    if (ast.isStringLiteralLikeNode(node) || ast.isNumericLiteral(node) || [K.TrueKeyword, K.FalseKeyword, K.NullKeyword].includes(node.kind)) {
      return clean;
    }
    if (ast.isPrefixUnaryExpression(node) && node.operator === K.ExclamationToken) {
      expression(node.operand, state, depth + 1);
      return clean;
    }
    if ((ast.isPrefixUnaryExpression(node) || ast.isPostfixUnaryExpression(node))
      && (node.operator === K.PlusPlusToken || node.operator === K.MinusMinusToken)) {
      const target = slot(node.operand, state, depth);
      if (!target) {
        issue(node, "unsupported-assignment", "This update target is outside the local binding or field model.");
        return clean;
      }
      const input = { ...(target.map.get(target.key) ?? initialField(target.key)), ref: undefined };
      write(target, input, node);
      return input;
    }
    if (ast.isArrayLiteralExpression(node)) {
      const ref = allocate(node, "array");
      let length = 0;
      for (const element of node.elements) {
        if (ast.isSpreadElement(element) || ast.isOmittedExpression(element)) {
          issue(element, "unsupported-array", "Spread elements and holes are outside the local array model.");
          return clean;
        }
        const input = expression(element, state, depth + 1);
        if (state.stopped || remaining < 0) return clean;
        write({ map: state.fields, key: `${ref}.${length++}` }, input, element, "element");
      }
      arrayLengths.set(ref, length);
      return { ...clean, ref };
    }
    if (ast.isObjectLiteralExpression(node)) {
      const ref = allocate(node, "object");
      for (const property of node.properties) {
        if (ast.isPropertyAssignment(property) && ast.isIdentifier(property.name)) {
          const input = expression(property.initializer, state, depth + 1);
          if (state.stopped) return clean;
          write({ map: state.fields, key: `${ref}.${property.name.text}` }, input, property, "field");
        } else {
          issue(property, "unsupported-object", "Only named properties in object literals are modeled.");
        }
      }
      return { ...clean, ref };
    }
    if (ast.isBinaryExpression(node)) {
      const operator = node.operatorToken.kind;
      if (operator === K.EqualsToken) {
        const target = slot(node.left, state, depth);
        const rhs = expression(node.right, state, depth + 1);
        if (state.stopped) return clean;
        if (target) write(target, rhs, node, "assignment");
        else issue(node.left, "unsupported-assignment", "This assignment target is outside the local binding or field model.");
        return rhs;
      }
      if ([K.PlusToken, K.EqualsEqualsEqualsToken, K.ExclamationEqualsEqualsToken, K.LessThanToken, K.GreaterThanToken].includes(operator)) {
        const a = expression(node.left, state, depth + 1);
        const b = expression(node.right, state, depth + 1);
        return operator === K.PlusToken ? { ...join(a, b), ref: undefined } : clean;
      }
    }
    issue(node, "unsupported-expression", `No flow model for ${K[node.kind]}.`);
    return clean;
  }

  function condition(node, state, depth) {
    if (!node) return { yes: state };
    expression(node, state, depth + 1);
    if (state.stopped) return {};
    let negated = false;
    while (ast.isParenthesizedExpression(node) || ast.isPrefixUnaryExpression(node) && node.operator === K.ExclamationToken) {
      if (ast.isParenthesizedExpression(node)) node = node.expression;
      else {
        node = node.operand;
        negated = !negated;
      }
    }
    if (node.kind === K.TrueKeyword || node.kind === K.FalseKeyword) {
      return (node.kind === K.TrueKeyword) !== negated ? { yes: state } : { no: state };
    }
    const tested = node;
    if (ast.isAwaitExpression(node)) node = unparenthesized(node.expression);
    const yes = fork(state);
    const guard = ast.isCallExpression(node) ? model(callee(node)) : undefined;
    if (guard?.kind === "guard") {
      const arg = node.arguments[guard.argument];
      // A promise is itself truthy. Only a boolean guard result authorizes
      // clearing the exact binding on the accepted branch.
      if (facts.typeIs(tested, "boolean") !== true) {
        issue(tested, "unresolved-guard", "The guard condition did not produce a resolved boolean.");
      } else if (arg && ast.isIdentifier(arg)) {
        const target = slot(arg, negated ? state : yes, depth + 1);
        if (target) write(target, { ...clean, ref: target.map.get(target.key)?.ref }, arg);
        else issue(arg, "unresolved-guard", "Could not identify the binding checked by the guard.");
      } else {
        issue(node, "unresolved-guard", "Only guards on a single local binding are supported.");
      }
    }
    return { yes, no: state };
  }

  function declareLocals(list, state, depth) {
    if (!(list.flags & NodeFlags.BlockScoped)) issue(list, "unsupported-var", "Only block-scoped variable declarations are modeled.");
    for (const declaration of list.declarations) {
      const input = expression(declaration.initializer, state, depth + 1);
      if (state.stopped) return;
      const id = ast.isIdentifier(declaration.name) && facts.symbol(declaration.name)?.id;
      if (id === false || id === undefined) issue(declaration, "unsupported-binding", "Expected a simple local variable binding.");
      else write({ map: state.locals, key: id }, input, declaration);
    }
  }

  function loop(node, state, exits, depth) {
    const counted = ast.isForStatement(node);
    const postTest = ast.isDoStatement(node);
    let elements;
    if (ast.isForOfStatement(node)) {
      const array = !node.awaitModifier && arrayOf(expression(node.expression, state, depth + 1));
      if (state.stopped) return undefined;
      const list = node.initializer;
      const name = ast.isVariableDeclarationList(list) && (list.flags & NodeFlags.BlockScoped) && list.declarations.length === 1
        ? list.declarations[0].name : undefined;
      const binding = name && ast.isIdentifier(name) ? facts.symbol(name) : undefined;
      if (!array || !binding) {
        issue(node, "unsupported-statement", "For-of requires a local array and a simple loop variable.");
        return state;
      }
      if (!array.length) return state;
      elements = { ...array, id: binding.id, name };
    }
    if (counted && node.initializer) {
      if (ast.isVariableDeclarationList(node.initializer)) declareLocals(node.initializer, state, depth + 1);
      else expression(node.initializer, state, depth + 1);
      if (state.stopped) return undefined;
    }
    const test = counted ? node.condition : node.expression;
    let head = state;
    let exit;
    loopDepth++;
    try {
      for (let iteration = 0; iteration < limits.loops; iteration++) {
        if (spent(node, depth)) return exit;
        const before = fork(head);
        const { yes, no } = elements || postTest ? { yes: before } : condition(test, before, depth);
        exit = merge(exit, no);
        if (!yes) return exit;
        if (elements) write({ map: yes.locals, key: elements.id }, fieldValue(yes, `${elements.ref}.${iteration}`), elements.name, "element");
        const control = {};
        const body = statement(node.statement, yes, exits, depth + 1, control);
        exit = merge(exit, control.break);
        let back = merge(body, control.continue);
        if (!back) return exit;
        if (elements) {
          if (iteration + 1 === elements.length) return merge(exit, back);
          head = back;
          continue;
        }
        if (postTest) {
          const split = condition(test, back, depth);
          exit = merge(exit, split.no);
          back = split.yes;
          if (!back) return exit;
        } else if (counted) {
          expression(node.incrementor, back, depth + 1);
          if (back.stopped) return exit;
        }
        const next = merge(head, back);
        if (sameState(head, next)) return exit;
        head = next;
      }
      issue(node, "loop-limit", elements ? "The array iteration reached its element budget."
        : "The loop reached its iteration budget before the flow facts stabilized.");
      return exit;
    } finally {
      loopDepth--;
    }
  }

  function recordReturn(node, input, state, exits) {
    if (state.stopped) return;
    if (input.ref?.startsWith("object:") || arrayLengths.has(input.ref)) {
      issue(node, "unresolved-heap-return", "Returning a newly allocated object or array is outside the summary model.");
    }
    exits.push({ state, input: input.flows.size ? append(input, step(node, "return", "Returned from this function.")) : input });
  }

  function statement(node, state, exits, depth, control) {
    if (state.stopped) return undefined;
    if (spent(node, depth)) return state;
    if (ast.isBlock(node)) {
      for (const child of node.statements) {
        state = statement(child, state, exits, depth + 1, control);
        if (!state || remaining < 0) break;
      }
    } else if (ast.isVariableStatement(node)) {
      declareLocals(node.declarationList, state, depth);
    } else if (ast.isExpressionStatement(node)) {
      expression(node.expression, state, depth + 1);
    } else if (ast.isIfStatement(node)) {
      const { yes, no } = condition(node.expression, state, depth);
      const taken = yes && statement(node.thenStatement, yes, exits, depth + 1, control);
      const other = no && node.elseStatement ? statement(node.elseStatement, no, exits, depth + 1, control) : no;
      return merge(taken, other);
    } else if (ast.isForStatement(node) || ast.isWhileStatement(node) || ast.isDoStatement(node) || ast.isForOfStatement(node)) {
      return loop(node, state, exits, depth);
    } else if (ast.isBreakStatement(node) || ast.isContinueStatement(node)) {
      if (!control || node.label) issue(node, "unsupported-jump", "This jump has no modeled loop target.");
      else {
        const target = ast.isBreakStatement(node) ? "break" : "continue";
        control[target] = merge(control[target], state);
      }
      return undefined;
    } else if (ast.isReturnStatement(node)) {
      recordReturn(node, expression(node.expression, state, depth + 1), state, exits);
      return undefined;
    } else if (ast.isThrowStatement(node)) {
      expression(node.expression, state, depth + 1);
      issue(node, "unsupported-throw", "Exception edges are outside the control-flow model.");
      return undefined;
    } else if (!ast.isFunctionDeclaration(node) && !ast.isEmptyStatement(node)) {
      issue(node, "unsupported-statement", `No control-flow model for ${K[node.kind]}.`);
    }
    return state?.stopped ? undefined : state;
  }

  function summarize(fn, callDepth, nesting) {
    if (callDepth > limits.calls || active.has(fn.id)) {
      issue(fn, "call-limit", "Recursion or the interprocedural depth limit was reached.");
      return undefined;
    }
    if (spent(fn, nesting)) return undefined;
    const key = `${callDepth}:${nesting}:${loopDepth > 0}:${fn.id}`;
    if (summaries.has(key)) return summaries.get(key);
    if (!fn.body || !facts.prepare(fn, limits.steps)) {
      issue(fn, "unavailable-body", "The function body is unavailable or exceeds the analysis budget.");
      return undefined;
    }
    if (facts.hasSyntaxErrors(fn)) {
      issue(fn, "syntax-errors", "The source file has syntax errors in this snapshot.");
      return undefined;
    }
    if (fn.asteriskToken) {
      issue(fn, "unsupported-function", "Generator iteration is outside the call model.");
      return undefined;
    }
    active.add(fn.id);
    callStack.push(callDepth);
    const events = new Map();
    eventStack.push(events);
    const state = { locals: new Map(), fields: new Map() };
    for (const [index, p] of [...fn.parameters].entries()) {
      const id = ast.isIdentifier(p.name) && facts.symbol(p.name)?.id;
      if (id !== false && id !== undefined && !p.dotDotDotToken && !p.initializer) state.locals.set(id, parameter(index));
      else issue(p, "unsupported-parameter", "Only simple positional parameters are modeled.");
    }
    const exits = [];
    if (ast.isBlock(fn.body)) {
      const fallthrough = statement(fn.body, state, exits, nesting);
      if (fallthrough) exits.push({ state: fallthrough, input: clean });
    } else {
      recordReturn(fn.body, expression(fn.body, state, nesting), state, exits);
    }
    let returned = exits[0]?.input ?? clean;
    let final = exits[0]?.state;
    for (const exit of exits.slice(1)) {
      returned = join(returned, exit.input);
      final = merge(final, exit.state);
    }
    const effects = new Map([...final?.fields ?? []].filter(([key]) => key.startsWith("arg:")));
    const summary = { returned, effects, events, returnsNormally: exits.length > 0 };
    if (remaining >= 0) summaries.set(key, summary);
    eventStack.pop();
    callStack.pop();
    active.delete(fn.id);
    return summary;
  }

  const events = summarize(root, 0, 0)?.events ?? new Map();
  const findings = [];
  for (const { site, call, model: sink, value: input } of events.values()) {
    const paths = [];
    for (const [key, path] of input.flows) {
      if (key.startsWith("arg:")) {
        const index = keyParts(key)[0];
        if (!rule.entryParameters.includes(index)) continue;
        paths.push(concatPath([step(root.parameters[index], "source", "External input enters through this parameter.")], path));
      } else paths.push(path);
    }
    if (!paths.length) continue;
    if (sink.type) {
      const eligible = facts.typeIs(site, sink.type);
      if (eligible === undefined) issue(site, "unresolved-type", `The argument's ${sink.type} type could not be determined.`);
      if (eligible !== true) continue;
    }
    if (sink.capture && !captures(call, site, sink.capture)) {
      issue(site, "unresolved-capture", "The sink argument did not resolve to the requested AST capture.");
      continue;
    }
    findings.push({ location: facts.location(site), path: paths.reduce((a, b) => b.length < a.length ? b : a) });
  }
  return { findings, issues: [...issues.values()] };
}
