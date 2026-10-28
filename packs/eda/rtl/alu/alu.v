// 8-bit ALU: op 0 add, 1 sub, 2 and, 3 or, 4 xor, 5 set-less-than (signed), 6 shift left, 7 shift right
// (logical, by b[2:0]). zero is set when y is 0.
module alu(input [7:0] a, input [7:0] b, input [2:0] op, output reg [7:0] y, output zero);
  always @(*) begin
    case (op)
      3'd0: y = a + b;
      3'd1: y = a - b;
      3'd2: y = a & b;
      3'd3: y = a | b;
      3'd4: y = a ^ b;
      3'd5: y = ($signed(a) < $signed(b)) ? 8'd1 : 8'd0;
      3'd6: y = a << b[2:0];
      default: y = a >> b[2:0];
    endcase
  end
  assign zero = (y == 8'd0);
endmodule
