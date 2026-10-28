// 8-bit Gray code converters: g is the Gray code of b; back is the binary value of the Gray code h.
module gray(input [7:0] b, output [7:0] g, input [7:0] h, output reg [7:0] back);
  assign g = b ^ (b >> 1);
  integer i;
  always @(*) begin
    back[7] = h[7];
    for (i = 6; i >= 0; i = i - 1)
      back[i] = back[i + 1] ^ h[i];
  end
endmodule
