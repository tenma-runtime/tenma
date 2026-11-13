<?php

namespace Development;

/** What `tenma --watch` tells the script about its reloads. */
final class Watch
{
    /**
     * The file whose change made the script run again, null on the first run and without
     * --watch. A framework can tell from it whether only a part of the app changed.
     */
    public static function reloadedBy(): ?string
    {
        return \tenma_reloaded_by();
    }

    /**
     * Tells --watch that the script is about to write $content into $file itself, such as a template
     * it compiled and loads: that change reloads nothing. Call it before the write (or the rename
     * that puts the file in place). Without --watch it does nothing.
     */
    public static function writing(string $file, string $content): void
    {
        \tenma_watch_writing($file, $content);
    }
}
