// Keep homepage examples aligned with the runnable CoreVM examples shipped in
// playground/web/playground.js. Native actor examples are intentionally excluded.
export const homepageExamples = [
  {
    "id": "hello",
    "label": "Hello world",
    "code": "// Welcome to the Nulang playground!\nperform IO.print(\"Hello, Nulang!\")\n\nlet name = \"World\"\nperform IO.print(\"Hello, \" + name + \"!\")\n\nlet answer = 42\nperform IO.print(\"The answer is \" + perform Int.to_string(answer))\n"
  },
  {
    "id": "recursion",
    "label": "Recursion",
    "code": "let rec fib = fn(n) {\n    if n < 2 then n else fib(n - 1) + fib(n - 2)\n}\n\nlet rec fact = fn(n) {\n    if n <= 1 then 1 else n * fact(n - 1)\n}\n\nperform IO.print(\"fib(10) = \" + perform Int.to_string(fib(10)))\nperform IO.print(\"5! = \" + perform Int.to_string(fact(5)))\n"
  },
  {
    "id": "closures",
    "label": "Closures",
    "code": "let add = fn(x, y) { x + y }\n\nlet make_adder = fn(n) { fn(x) { add(x, n) } }\nlet add10 = make_adder(10)\n\nperform IO.print(\"add10(32) = \" + perform Int.to_string(add10(32)))\n\nlet result = {\n    let a = 10\n    let b = 20\n    a + b\n}\nperform IO.print(\"Block result: \" + perform Int.to_string(result))\n"
  }
];
