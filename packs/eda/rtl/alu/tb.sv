module tb;
  logic [7:0] a, b; logic [2:0] op; wire [7:0] y; wire zero;
  alu dut(.a(a), .b(b), .op(op), .y(y), .zero(zero));
  logic [15:0] r = 16'hACE1;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  function automatic logic [7:0] model(input logic [7:0] a, b, input logic [2:0] op);
    case (op)
      0: return a + b; 1: return a - b; 2: return a & b; 3: return a | b; 4: return a ^ b;
      5: return ($signed(a) < $signed(b)) ? 1 : 0; 6: return a << b[2:0]; default: return a >> b[2:0];
    endcase
  endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(r)); a = r[7:0]; r = step(step(r)); b = r[7:0]; op = i[2:0];
      if (i % 37 == 0) b = a;
      #1;
      total++; if (y === model(a, b, op) && zero === (model(a, b, op) == 0)) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
