package example;

import java.util.List;

/** A small syntax and editing fixture. */
public final class Example {
    public static void main(String[] args) {
        var values = List.of("Hello", "日本語", "Kwil");
        for (String value : values) {
            System.out.println(value + " — " + value.length());
        }
    }
}
