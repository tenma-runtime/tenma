<?php

$finder = PhpCsFixer\Finder::create()
    ->in([__DIR__ . '/stdlib', __DIR__ . '/demo'])
    ->exclude('vendor');

return (new PhpCsFixer\Config())
    ->setRules([
        '@PER-CS2.0' => true,
        // House style: `fn ($x) =>` with a space, as in Io
        'function_declaration' => ['closure_fn_spacing' => 'one'],
        // Each file of stdlib/ came with every import of the namespace block it was cut from
        'no_unused_imports' => true,
    ])
    ->setFinder($finder);
