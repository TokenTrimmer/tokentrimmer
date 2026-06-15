import openai

resp = openai.chat.completions.create(
    model="gpt-4o",
    max_tokens=200,
    messages=[
        {"role": "system", "content": """## Section 0
This section explains concept number 0 in plain language for the reader.
It continues with additional supporting detail about topic 0 and why it matters.

## Section 1
This section explains concept number 1 in plain language for the reader.
It continues with additional supporting detail about topic 1 and why it matters.

## Section 2
This section explains concept number 2 in plain language for the reader.
It continues with additional supporting detail about topic 2 and why it matters.

## Section 3
This section explains concept number 3 in plain language for the reader.
It continues with additional supporting detail about topic 3 and why it matters.

## Section 4
This section explains concept number 4 in plain language for the reader.
It continues with additional supporting detail about topic 4 and why it matters.

## Section 5
This section explains concept number 5 in plain language for the reader.
It continues with additional supporting detail about topic 5 and why it matters.

## Section 6
This section explains concept number 6 in plain language for the reader.
It continues with additional supporting detail about topic 6 and why it matters.

## Section 7
This section explains concept number 7 in plain language for the reader.
It continues with additional supporting detail about topic 7 and why it matters.

## Section 8
This section explains concept number 8 in plain language for the reader.
It continues with additional supporting detail about topic 8 and why it matters.

## Section 9
This section explains concept number 9 in plain language for the reader.
It continues with additional supporting detail about topic 9 and why it matters.

## Section 10
This section explains concept number 10 in plain language for the reader.
It continues with additional supporting detail about topic 10 and why it matters.

## Section 11
This section explains concept number 11 in plain language for the reader.
It continues with additional supporting detail about topic 11 and why it matters.

## Section 12
This section explains concept number 12 in plain language for the reader.
It continues with additional supporting detail about topic 12 and why it matters.

## Section 13
This section explains concept number 13 in plain language for the reader.
It continues with additional supporting detail about topic 13 and why it matters.

## Section 14
This section explains concept number 14 in plain language for the reader.
It continues with additional supporting detail about topic 14 and why it matters.

## Section 15
This section explains concept number 15 in plain language for the reader.
It continues with additional supporting detail about topic 15 and why it matters.

## Section 16
This section explains concept number 16 in plain language for the reader.
It continues with additional supporting detail about topic 16 and why it matters.

## Section 17
This section explains concept number 17 in plain language for the reader.
It continues with additional supporting detail about topic 17 and why it matters.

## Section 18
This section explains concept number 18 in plain language for the reader.
It continues with additional supporting detail about topic 18 and why it matters.

## Section 19
This section explains concept number 19 in plain language for the reader.
It continues with additional supporting detail about topic 19 and why it matters.

## Section 20
This section explains concept number 20 in plain language for the reader.
It continues with additional supporting detail about topic 20 and why it matters.

## Section 21
This section explains concept number 21 in plain language for the reader.
It continues with additional supporting detail about topic 21 and why it matters.

## Section 22
This section explains concept number 22 in plain language for the reader.
It continues with additional supporting detail about topic 22 and why it matters.

## Section 23
This section explains concept number 23 in plain language for the reader.
It continues with additional supporting detail about topic 23 and why it matters.

## Section 24
This section explains concept number 24 in plain language for the reader.
It continues with additional supporting detail about topic 24 and why it matters.

## Section 25
This section explains concept number 25 in plain language for the reader.
It continues with additional supporting detail about topic 25 and why it matters.

## Section 26
This section explains concept number 26 in plain language for the reader.
It continues with additional supporting detail about topic 26 and why it matters.

## Section 27
This section explains concept number 27 in plain language for the reader.
It continues with additional supporting detail about topic 27 and why it matters.

## Section 28
This section explains concept number 28 in plain language for the reader.
It continues with additional supporting detail about topic 28 and why it matters.

## Section 29
This section explains concept number 29 in plain language for the reader.
It continues with additional supporting detail about topic 29 and why it matters.

## Section 30
This section explains concept number 30 in plain language for the reader.
It continues with additional supporting detail about topic 30 and why it matters.

## Section 31
This section explains concept number 31 in plain language for the reader.
It continues with additional supporting detail about topic 31 and why it matters.

## Section 32
This section explains concept number 32 in plain language for the reader.
It continues with additional supporting detail about topic 32 and why it matters.

## Section 33
This section explains concept number 33 in plain language for the reader.
It continues with additional supporting detail about topic 33 and why it matters.

## Section 34
This section explains concept number 34 in plain language for the reader.
It continues with additional supporting detail about topic 34 and why it matters.

## Section 35
This section explains concept number 35 in plain language for the reader.
It continues with additional supporting detail about topic 35 and why it matters.

## Section 36
This section explains concept number 36 in plain language for the reader.
It continues with additional supporting detail about topic 36 and why it matters.

## Section 37
This section explains concept number 37 in plain language for the reader.
It continues with additional supporting detail about topic 37 and why it matters.

## Section 38
This section explains concept number 38 in plain language for the reader.
It continues with additional supporting detail about topic 38 and why it matters.

## Section 39
This section explains concept number 39 in plain language for the reader.
It continues with additional supporting detail about topic 39 and why it matters.
"""},
    ],
)
print(resp)
