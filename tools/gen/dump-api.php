<?php
// Dumps the API surface of the loaded mongodb extension (Reflection) as JSON: classes with
// flags, parents, interfaces, constants (values), properties, methods (flags, params, return
// types), functions and constants. Used by gen_api.py for constant values and to diff the API
// surface of two builds:  php -n -d extension=<so> dump-api.php > api.json
$ext = new ReflectionExtension('mongodb');
$type = static fn (?ReflectionType $t) => $t === null ? null : (string) $t;
$params = static function (ReflectionFunctionAbstract $f) use ($type) {
    $r = [];
    foreach ($f->getParameters() as $p) {
        $r[] = ['name' => $p->getName(), 'type' => $type($p->getType()), 'optional' => $p->isOptional(),
            'byref' => $p->isPassedByReference(), 'variadic' => $p->isVariadic(),
            'default' => $p->isDefaultValueAvailable() ? var_export($p->getDefaultValue(), true) : null];
    }
    return $r;
};
$out = ['version' => $ext->getVersion(), 'classes' => [], 'functions' => [], 'constants' => $ext->getConstants(), 'ini' => $ext->getINIEntries()];
foreach ($ext->getClasses() as $c) {
    if (str_starts_with($c->getName(), 'Revolt\\')) {
        continue; // the embedded revolt-rs's (registered at MINIT unless another copy did), not the MongoDB API
    }
    $m = [];
    foreach ($c->getMethods() as $meth) {
        if ($meth->getDeclaringClass()->getName() !== $c->getName()) { continue; }
        $m[$meth->getName()] = ['modifiers' => $meth->getModifiers(), 'params' => $params($meth), 'return' => $type($meth->getReturnType()),
            'tentative' => $type($meth->getTentativeReturnType()), 'required' => $meth->getNumberOfRequiredParameters(),
            'deprecated' => $meth->isDeprecated()];
    }
    $consts = [];
    foreach ($c->getReflectionConstants() as $k) {
        if ($k->getDeclaringClass()->getName() !== $c->getName()) { continue; }
        $consts[$k->getName()] = ['value' => $k->getValue() instanceof UnitEnum ? null : $k->getValue(), 'modifiers' => $k->getModifiers(), 'case' => $k->isEnumCase()];
    }
    $props = [];
    foreach ($c->getProperties() as $p) {
        if ($p->getDeclaringClass()->getName() !== $c->getName()) { continue; }
        $props[$p->getName()] = ['type' => $type($p->getType()), 'modifiers' => $p->getModifiers(), 'default' => $p->hasDefaultValue() ? var_export($p->getDefaultValue(), true) : null];
    }
    $out['classes'][$c->getName()] = ['interface' => $c->isInterface(), 'enum' => $c->isEnum(), 'modifiers' => $c->getModifiers(),
        'parent' => $c->getParentClass() ? $c->getParentClass()->getName() : null, 'interfaces' => $c->getInterfaceNames(),
        'constants' => $consts, 'properties' => $props, 'methods' => $m, 'attributes' => array_map(fn ($a) => $a->getName(), $c->getAttributes())];
}
foreach ($ext->getFunctions() as $f) {
    $out['functions'][$f->getName()] = ['params' => $params($f), 'return' => $type($f->getReturnType()), 'required' => $f->getNumberOfRequiredParameters()];
}
echo json_encode($out, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_PRESERVE_ZERO_FRACTION), "\n";
